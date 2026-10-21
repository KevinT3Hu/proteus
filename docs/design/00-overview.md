# Proteus — Product Overview

Status: Draft v1 · Last updated: 2026-10-20

## Positioning

A fast, native Wayland terminal where panes, files, images and AI share one
context. Everything works from the keyboard, and the AI is fully optional.

## Target user

Linux developer living in nvim/helix, git and ssh, who today stacks
tmux/zellij + yazi + an AI CLI on top of their terminal.

## Principles

1. Terminal correctness and input latency come before features.
2. Keyboard-first; the mouse is supported but secondary.
3. Good defaults; no config file required.
4. Nothing leaves the machine without the user's explicit consent.
5. Built-in features use the same plugin API as third parties ("dogfood the API").

## Platform scope

- Linux, Wayland only. No X11, macOS or Windows.
- Required compositor protocols: `xdg-shell`, `text-input-v3`,
  `wp-fractional-scale-v1`, `wp-viewporter`. Optional: `xdg-decoration`,
  `cursor-shape-v1`, `xdg-activation-v1`, `primary-selection`.
- Single process. No detachable sessions; session restore recreates layout
  and cwd only.
- CJK is a first-class requirement (IME, wide chars, per-language fallback).

## Naming

| Item | Value |
|---|---|
| Binary | `proteus` |
| Config | `$XDG_CONFIG_HOME/proteus/init.lua` |
| Data / plugins | `$XDG_DATA_HOME/proteus/` |
| State (session, logs) | `$XDG_STATE_HOME/proteus/` |
| Runtime | `$XDG_RUNTIME_DIR/proteus/` |
| `TERM` | `proteus` (own terminfo), fallback `xterm-256color` over ssh |
| App ID | `dev.proteus.Proteus` (placeholder until a domain is chosen) |
| Rust crates | `proteus-*` (the bare `proteus` name on crates.io is taken by an unrelated crate) |

## Feature documents

| # | Document | Scope |
|---|---|---|
| 1 | [01-core.md](01-core.md) | Terminal emulation, shell integration, blocks, search, hints, palette |
| 2 | [02-multiplexer.md](02-multiplexer.md) | Tabs, panes, modes, layouts, chrome |
| 3 | [03-file-manager.md](03-file-manager.md) | File manager (first-party plugin) |
| 4 | [04-graphics.md](04-graphics.md) | Kitty graphics, Sixel, iTerm2, kitty keyboard protocol |
| 5 | [05-scripting.md](05-scripting.md) | Lua config, WASM plugins, plugin UI API, permissions |
| 6 | [06-ai.md](06-ai.md) | AI features, providers, privacy |

## Decision log

| # | Decision | Choice |
|---|---|---|
| D1 | Name | Proteus |
| D2 | Command blocks UI | Classic contiguous output with gutter marks |
| D3 | Default keyboard model | Modes with hint bar (zellij-style) |
| D4 | Tab bar / status bar | Native GUI rendering, styled to blend with the cell grid |
| D5 | File manager | WASM plugin from day one |
| D6 | Config format | Lua only |
| D7 | Lua version | Lua 5.4 |
| D8 | AI default | Ask on first run |
| D9 | AI agent mode | v2 |
| D10 | Quake dropdown | Not planned |
| D11 | Rendering | `wgpu` (Vulkan primary, GL fallback backend) |

### Consequence of D5

Shipping the file manager as a plugin means the plugin UI API, filesystem
capability, image placement and background-task support must exist in v1,
not later. Scripting (doc 05) therefore moves ahead of the file manager in the
roadmap, and the file manager is the acceptance test for the plugin API.

## Roadmap

| Milestone | Content | Exit criteria |
|---|---|---|
| M1 Core | One window, one pane: wgpu rendering, fonts, xkb, IME, clipboard | vttest basics; nvim/htop correct; fcitx5 preedit works on KWin, Sway, Mutter; renders on Intel, AMD and NVIDIA (proprietary) with both Vulkan and GL backends |
| M2 Mux | Tabs, splits, floating/stacked panes, modes, chrome | Daily-drivable replacement for zellij without plugins |
| M3 Shell integration | OSC 133/7, blocks, search, hints, palette, notifications | Jump/copy blocks in bash, zsh, fish |
| M4 Graphics | Kitty graphics + keyboard protocol, Sixel, iTerm2 | Images correct across splits, scroll, resize |
| M5 Scripting | Lua config, WASM host, plugin UI API, permissions | Example plugin with sidebar UI loads/unloads at runtime |
| M6 File manager | First-party plugin | All features in doc 03 without host special-casing |
| M7 AI | First-party plugins: core + providers | Features 1–3 of doc 06 with local and hosted models |
| v2 | AI agent mode, SFTP in file manager, plugin registry | — |
