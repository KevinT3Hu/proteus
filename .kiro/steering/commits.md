# Commits

Before every commit:

1. `cargo fmt` (uses `rustfmt.toml` at the repo root), then `cargo fmt --check` must pass.
2. `cargo build`, `cargo clippy --all-targets` (no warnings) and `cargo test` must pass.

Do not hand-format code against `rustfmt.toml`; let `cargo fmt` decide.
