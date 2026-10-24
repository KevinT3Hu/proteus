-- Ordinary Lua config: these modules have no manifests or plugin lifecycle.
local p = require("proteus")
require("extensions.clock")
require("extensions.panel")

-- The same APIs work inline; later config calls override module defaults.
p.config { font = { size = 13 } }
p.on("command_finished", function(ev)
  if ev.exit_code ~= nil and ev.exit_code ~= 0 then p.notify("Command failed: " .. (ev.command or "(unknown command)")) end
end)

-- Build and cache the file manager from Rust source. Paths are relative to
-- this config directory; update this path if you copy the config elsewhere.
p.load_plugin {
  source = "../../plugins/files",
  config = { position = "left", width = 32 }, -- file manager settings (columns)
}

-- Additional plugins can be loaded with separate calls:
-- p.load_plugin { source = "https://example.com/my-plugin.git", rev = "COMMIT", subdir = "plugin" }
-- p.load_plugin { source = "~/plugins/tool.wasm", id = "my.tool", permissions = { ui = true } }
