# Example plugins

Plugins are WebAssembly components (`wasm32-wasip2`) using the SDK in
`crates/proteus-plugin` (API: `crates/proteus-plugin/wit/proteus.wit`).
They are not part of the terminal's Cargo workspace.

```sh
rustup target add wasm32-wasip2
cd examples/plugins/pane-list
cargo build --release --target wasm32-wasip2
proteus +plugin install .        # shows the permissions and asks
```

In a running terminal, open the palette (Ctrl+Shift+P) and choose
"Pane list: toggle". `proteus +plugin disable example.pane-list` unloads it
at once; `enable` loads it again.
