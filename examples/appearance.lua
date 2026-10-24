-- Copy to ~/.config/proteus/init.lua. Every color is explicit; no preset is needed.
local p = require("proteus")
p.config {
  theme = {
    foreground = "#cdd6f4", background = "#1e1e2e", cursor = "#f5e0dc",
    ansi = {
      "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
      "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
      "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
      "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
    },
    indexed = { [208] = "#fab387" },
    ui = {
      foreground = "#cdd6f4", background = "#181825", accent = "#89b4fa",
      bar = "#313244", dim = "#6c7086",
      selection_foreground = "#cdd6f4", selection_background = "#45475a",
      search_foreground = "#1e1e2e", search_background = "#f9e2af",
      search_current_background = "#fab387", cursor_text = "#1e1e2e", bell = "#f5e0dc",
    },
  },
  font = {
    family = "JetBrains Mono", size = 13, ligatures = true,
    bold_family = "JetBrains Mono", italic_family = "JetBrains Mono",
    bold_italic_family = "JetBrains Mono",
    -- Logical pixels. false uses the natural metrics of the font.
    cell_width = false, cell_height = false,
    letter_spacing = 0, line_spacing = 2, offset_x = 0, offset_y = 0,
  },
  -- Initial logical pixel size. Alternatively: columns = 120, rows = 36.
  window = { width = 1200, height = 800 },
  cursor = { shape = "bar", blink = true },
  ui = {
    padding = { horizontal = 12, vertical = 8 }, pane_gap = 8, border_width = 1,
    tab_bar = "always", hint_bar = "in-mode", pane_titles = "auto", gutter = 0.5,
  },
}
p.status.segment("right", function() return os.date("%H:%M") end)
