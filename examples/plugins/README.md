# WASM plugin sources

WASM plugins use the SDK in `crates/proteus-plugin`. Load their source paths
or URLs directly in `~/.config/proteus/init.lua`; there is no install step:

```lua
require("proteus").load_plugin {
  source = "~/code/proteus/examples/plugins/pane-list",
}
```

Use the actual checkout path. Cargo and the `wasm32-wasip2` Rust target must be
available; Proteus compiles and caches the component automatically. The palette
then contains "Pane list: toggle". Its stable action reference is
`plugin:example.pane-list:sidebar.toggle`; use it in `keys.normal` or `p.keymap`
to bind another key, and set `["alt+p"] = false` to remove the default. Remove the
`p.load_plugin` call to stop the plugin.

Lua extensions are ordinary config code, either inline in `init.lua` or loaded
with `require`. They have no plugin manifest or registration. See
[`../config`](../config) for modular config examples and custom UI.
