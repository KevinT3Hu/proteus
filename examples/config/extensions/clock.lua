local p = require("proteus")
p.status.segment("right", function() return os.date("%H:%M:%S") end)
