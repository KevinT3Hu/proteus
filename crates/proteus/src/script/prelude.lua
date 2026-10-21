-- The Lua half of the `proteus` module (see script/mod.rs). `p` is the
-- module table; `call(op, ...)` reaches the terminal while a callback runs.
local p, call, start_timer, stop_timer = ...

local Pane = {}
local methods = {}

Pane.__index = function(self, k)
  local m = methods[k]
  if m ~= nil then
    return m
  end
  local info = call("pane_info", rawget(self, "id"))
  return info and info[k]
end
Pane.__newindex = function(_, k)
  error("pane." .. tostring(k) .. " is read-only", 2)
end
Pane.__eq = function(a, b)
  return rawget(a, "id") == rawget(b, "id")
end
Pane.__tostring = function(self)
  return "pane " .. tostring(rawget(self, "id"))
end

local function pane(id)
  if id == nil then
    return nil
  end
  return setmetatable({ id = id }, Pane)
end

local function id_of(self)
  local id = type(self) == "table" and rawget(self, "id")
  if id == nil then
    error("expected a pane (use pane:method(), not pane.method())", 3)
  end
  return id
end

--- All metadata at once (nil once the pane is gone).
function methods:info()
  return call("pane_info", id_of(self))
end
function methods:exists()
  return call("pane_info", id_of(self)) ~= nil
end
function methods:split(opts)
  return pane(call("split", id_of(self), opts or {}))
end
function methods:send_text(text)
  call("send_text", id_of(self), tostring(text), false)
end
function methods:insert_text(text)
  call("send_text", id_of(self), tostring(text), true)
end
function methods:close()
  call("close", id_of(self))
end
function methods:focus()
  call("focus", id_of(self))
end
function methods:set_name(name)
  call("set_name", id_of(self), name)
end
function methods:read_screen(opts)
  return call("read_screen", id_of(self), opts and opts.scrollback or 0)
end
function methods:read_blocks()
  return call("read_blocks", id_of(self))
end
function methods:block_text(block, part)
  return call("block_text", id_of(self), block, part or "output")
end

p.panes = {
  list = function()
    local out = {}
    for i, id in ipairs(call("pane_ids")) do
      out[i] = pane(id)
    end
    return out
  end,
  focused = function()
    return pane(call("focused"))
  end,
  get = function(id)
    if call("pane_info", id) ~= nil then
      return pane(id)
    end
    return nil
  end,
  new_tab = function(opts)
    return pane(call("new_tab", opts or {}))
  end,
  float = function(opts)
    return pane(call("float", opts or {}))
  end,
}

function p.notify(text, opts)
  call("notify", tostring(text), opts or {})
end

function p.run_action(name)
  call("run_action", name)
end

local Timer = {}
Timer.__index = Timer
function Timer:stop()
  stop_timer(self.id)
end

function p.after(secs, f)
  return setmetatable({ id = start_timer(secs, f, false) }, Timer)
end
function p.every(secs, f)
  return setmetatable({ id = start_timer(secs, f, true) }, Timer)
end

return pane
