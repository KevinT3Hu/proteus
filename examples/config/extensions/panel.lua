local p = require("proteus")
p.keymap("normal", "alt+i", function()
  p.ui.open {
    title = "Config panel", cols = 40, rows = 4,
    on_event = function(ev, surface)
      if ev.type == "resize" then
        surface:draw { "This UI is ordinary Lua config.", "Press Escape to close." }
      elseif ev.type == "key" and ev.key == "escape" then
        surface:close()
      end
    end,
  }
end)
