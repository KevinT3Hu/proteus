//! Declarative project tasks. Discovery and parsing never execute project code.
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    path::{Path, PathBuf},
};

pub const FILE: &str = ".proteus-workspace";
const MAX_FILE: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub path: PathBuf,
    pub name: String,
    pub tasks: Vec<Task>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    name: Option<String>,
    #[serde(default)]
    presets: Vec<String>,
    #[serde(default)]
    tasks: BTreeMap<String, TaskDefinition>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TaskDefinition {
    name: Option<String>,
    command: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    disabled: bool,
}

fn read(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{}: expected a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(format!("{}: exceeds 256 KiB", path.display()));
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

impl Workspace {
    pub fn discover(cwd: &Path) -> Result<Option<Self>, String> {
        use std::os::unix::fs::MetadataExt;
        let uid = rustix::process::geteuid().as_raw();
        for parent in cwd.ancestors() {
            let path = parent.join(FILE);
            match std::fs::symlink_metadata(&path) {
                Ok(m) => {
                    // Tasks run commands: another user's file (say in /tmp),
                    // or one others can change, defines nothing for us.
                    let dir = std::fs::metadata(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                    let others_replace = dir.mode() & 0o002 != 0 && dir.mode() & 0o1000 == 0;
                    let trusted = (m.uid() == uid || m.uid() == 0) && m.mode() & 0o002 == 0 && !others_replace;
                    // `load` reports symlinks (which it never follows) as errors.
                    if trusted || m.file_type().is_symlink() {
                        return Self::load(&path).map(Some);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(None)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let root = path.parent().ok_or("workspace has no parent")?.canonicalize().map_err(|e| e.to_string())?;
        Self::parse(
            &root,
            &read(path)?,
            |name| read(&root.join(name)),
            |name| root.join(name).is_file(),
            |relative| {
                let cwd = root.join(relative).canonicalize().map_err(|e| e.to_string())?;
                if !cwd.starts_with(&root) || !cwd.is_dir() {
                    return Err("cwd must stay inside workspace".into());
                }
                Ok(cwd)
            },
        )
    }

    /// Shared parser for local files and a bounded remote snapshot. Remote cwd
    /// paths are checked again on the server immediately before starting a task.
    pub fn parse(
        root: &Path,
        definition: &str,
        read_file: impl Fn(&str) -> Result<String, String>,
        exists: impl Fn(&str) -> bool,
        resolve: impl Fn(&Path) -> Result<PathBuf, String>,
    ) -> Result<Self, String> {
        if definition.len() as u64 > MAX_FILE {
            return Err("workspace exceeds 256 KiB".into());
        }
        let def: Definition = toml::from_str(definition).map_err(|e| e.to_string())?;
        let name = def.name.unwrap_or_else(|| root.file_name().unwrap_or_default().to_string_lossy().into_owned());
        if !text(&name) {
            return Err("workspace name must be nonempty text".into());
        }
        let mut tasks = BTreeMap::<String, TaskDefinition>::new();
        for preset in def.presets {
            match preset.as_str() {
                "rust" => {
                    for (id, args) in [
                        ("build", vec!["build"]),
                        ("test", vec!["test"]),
                        ("check", vec!["check"]),
                        ("format", vec!["fmt"]),
                        ("lint", vec!["clippy"]),
                    ] {
                        tasks.insert(
                            id.into(),
                            TaskDefinition {
                                command: Some(std::iter::once("cargo").chain(args).map(String::from).collect()),
                                ..Default::default()
                            },
                        );
                    }
                }
                "node" => {
                    let package: serde_json::Value =
                        serde_json::from_str(&read_file("package.json")?).map_err(|e| e.to_string())?;
                    let manager = if exists("pnpm-lock.yaml") {
                        "pnpm"
                    } else if exists("yarn.lock") {
                        "yarn"
                    } else if exists("bun.lock") || exists("bun.lockb") {
                        "bun"
                    } else {
                        "npm"
                    };
                    if let Some(scripts) = package.get("scripts").and_then(|v| v.as_object()) {
                        for (id, script) in scripts {
                            if !script.is_string() || !text(id) || id.starts_with('-') {
                                return Err("invalid package script".into());
                            }
                            tasks.insert(
                                id.clone(),
                                TaskDefinition {
                                    command: Some(vec![manager.into(), "run".into(), id.clone()]),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
                _ => return Err(format!("unknown task preset {preset:?}; expected rust or node")),
            }
        }
        // Project definitions replace complete preset entries by stable ID.
        tasks.extend(def.tasks);
        if tasks.len() > 256 {
            return Err("workspace exceeds 256 tasks".into());
        }
        let mut result = Vec::new();
        for (id, task) in tasks {
            if !text(&id) {
                return Err("invalid task ID".into());
            }
            if task.disabled {
                continue;
            }
            let command = task.command.ok_or_else(|| format!("task {id}: missing command"))?;
            if command.is_empty()
                || command.len() > 128
                || command.iter().any(|a| a.len() > 16384 || a.chars().any(char::is_control))
                || command[0].is_empty()
            {
                return Err(format!("task {id}: expected a nonempty command array"));
            }
            let name = task.name.unwrap_or_else(|| id.clone());
            if !text(&name) {
                return Err(format!("task {id}: invalid name"));
            }
            let relative = task.cwd.unwrap_or_default();
            if relative.is_absolute() {
                return Err(format!("task {id}: cwd must be relative"));
            }
            let cwd = resolve(&relative).map_err(|e| format!("task {id} cwd: {e}"))?;
            result.push(Task { id, name, command, cwd });
        }
        Ok(Self { path: root.join(FILE), name, tasks: result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_overrides_discovery_and_validation() {
        let root = std::env::temp_dir().join(format!("proteus-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let path = root.join(FILE);
        std::fs::write(&path, "name = 'Example'\npresets = ['rust']\n[tasks.test]\ncommand = ['cargo', 'test', '--workspace']\n[tasks.format]\ndisabled = true\n").unwrap();
        let workspace = Workspace::discover(&root.join("src")).unwrap().unwrap();
        assert_eq!(workspace.name, "Example");
        assert_eq!(workspace.tasks.len(), 4);
        assert_eq!(workspace.tasks.iter().find(|t| t.id == "test").unwrap().command, ["cargo", "test", "--workspace"]);
        std::fs::write(root.join("package.json"), r#"{"scripts":{"dev":"echo hi","test":"exit 1"}}"#).unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(&path, "presets = ['node']").unwrap();
        let workspace = Workspace::load(&path).unwrap();
        assert_eq!(workspace.tasks[0].command, ["pnpm", "run", "dev"]);
        for invalid in
            ["presets = ['unknown']", "typo = true", "[tasks.x]\ncommand=[]", "[tasks.x]\ncommand=['pwd']\ncwd='..'"]
        {
            std::fs::write(&path, invalid).unwrap();
            assert!(Workspace::load(&path).is_err(), "{invalid}");
        }
        std::fs::write(&path, "x".repeat(MAX_FILE as usize + 1)).unwrap();
        assert!(Workspace::load(&path).unwrap_err().contains("exceeds 256 KiB"));
        std::fs::remove_file(&path).unwrap();
        // A file others may write defines no tasks.
        std::fs::write(&path, "presets = ['node']").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Workspace::discover(&root.join("src")).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(root.join("package.json"), &path).unwrap();
        assert!(Workspace::discover(&root.join("src")).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(Workspace::load(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
