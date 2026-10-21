//! End-to-end: real shells in a PTY with injected shell integration, fed
//! through the core emulator; checks the resulting command blocks.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use proteus_core::{BlockPart, Config, Event, Terminal};
use proteus_pty::shell;
use proteus_pty::{CommandSpec, Pty, WindowSize};

/// Run `shell`, typing each input once a prompt is shown (or output went
/// quiet), and return a report of the blocks.
fn run_shell(shell: &str, inputs: &[&str]) -> Option<String> {
    if !Path::new(shell).exists() {
        eprintln!("skipping: {shell} not installed");
        return None;
    }
    // Isolated HOME so the user's rc files don't affect the result.
    let tag = format!("{}-{}", std::process::id(), shell.replace('/', "_"));
    let home = std::env::temp_dir().join(format!("proteus-e2e-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    let mut spec = CommandSpec::new(shell)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_RUNTIME_DIR", &home);
    spec.remove_env("ZDOTDIR");
    let dir = home.join("shell-integration");
    shell::install(&dir).unwrap();
    assert!(shell::inject(&mut spec, &dir).is_some(), "no integration for {shell}");

    let pty = Pty::spawn(&spec, WindowSize::new(80, 20)).unwrap();
    let mut reader = pty.try_clone_master().unwrap();
    let mut writer = pty.try_clone_master().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut term = Terminal::new(80, 20, Config::default());
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut inputs = inputs.iter();
    let (mut prompt, mut last) = (false, Instant::now());
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(b) => {
                term.advance(&b);
                last = Instant::now();
                let reply = term.take_pty_output();
                writer.write_all(&reply).unwrap();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        prompt |= term.take_events().iter().any(|e| matches!(e, Event::PromptStarted { .. }));
        let quiet = last.elapsed();
        if ((prompt && quiet >= Duration::from_millis(50)) || quiet >= Duration::from_millis(400))
            && let Some(i) = inputs.next()
        {
            writer.write_all(i.as_bytes()).unwrap();
            prompt = false;
            last = Instant::now();
        }
    }
    drop(pty);
    let _ = std::fs::remove_dir_all(&home);

    let mut out = String::new();
    for b in term.blocks().iter() {
        let exit = b.exit_code.map_or("-".to_string(), |c| c.to_string());
        out += &format!(
            "exit={exit} cwd={} command={:?}\n",
            b.cwd.as_deref().unwrap_or("-"),
            b.command.as_deref().unwrap_or("")
        );
        for line in term.block_text(b.id, BlockPart::Output).unwrap_or_default().lines() {
            out += &format!("    | {line}\n");
        }
    }
    Some(out)
}

fn check_blocks(text: &str) {
    assert!(text.contains("exit=0") && text.contains("command=\"echo one\""), "{text}");
    assert!(text.contains("    | one"), "{text}");
    assert!(text.contains("exit=3") && text.contains("command=\"(exit 3)\""), "{text}");
    assert!(text.contains("cwd=/tmp"), "{text}");
}

#[test]
fn bash_blocks() {
    if let Some(t) = run_shell("/bin/bash", &["echo one\r", "(exit 3)\r", "cd /tmp\r", "true\r", "exit\r"]) {
        check_blocks(&t);
    }
}

#[test]
fn zsh_blocks() {
    if let Some(t) = run_shell("/bin/zsh", &["echo one\r", "(exit 3)\r", "cd /tmp\r", "true\r", "exit\r"]) {
        check_blocks(&t);
    }
}

#[test]
fn fish_blocks() {
    // fish has no `(exit 3)` subshell syntax.
    if let Some(t) = run_shell("/usr/bin/fish", &["echo one\r", "fish -c 'exit 3'\r", "cd /tmp\r", "true\r", "exit\r"]) {
        assert!(t.contains("command=\"echo one\"") && t.contains("    | one"), "{t}");
        assert!(t.contains("exit=3"), "{t}");
        assert!(t.contains("cwd=/tmp"), "{t}");
    }
}
