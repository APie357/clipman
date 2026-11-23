-- bookmarks: mark moments in a clip (Ctrl+B), shown on the timeline and kept in the
-- clip's sidecar, with its own "Bookmarks" menu: jump between bookmarks, trim around
-- the nearest one, remove one or all.

local function load(path)
  return (clipman.meta.get(path) or {}).marks or {}
end

local function save(path, marks)
  clipman.meta.set(path, #marks > 0 and { marks = marks } or nil)
end

local function show(clip)
  clip:clear_markers()
  for i, t in ipairs(load(clip.path)) do
    clip:add_marker{ t = t, label = string.format("Bookmark %d (%.1f s)", i, t),
                     color = "#f5a623" }
  end
end

-- index of the bookmark nearest the playhead, or nil
local function nearest(clip, marks)
  local best
  for i, t in ipairs(marks) do
    if not best or math.abs(t - clip.playhead) < math.abs(marks[best] - clip.playhead) then
      best = i
    end
  end
  return best
end

-- jump to the next (dir = 1) or previous (dir = -1) bookmark
local function jump(clip, dir)
  local marks = load(clip.path)
  -- a little slack, so the bookmark the playhead is on doesn't count
  local pick
  for _, t in ipairs(marks) do
    if dir > 0 and t > clip.playhead + 0.05 and (not pick or t < pick) then pick = t end
    if dir < 0 and t < clip.playhead - 0.05 and (not pick or t > pick) then pick = t end
  end
  if pick then
    clip:seek(pick)
  else
    clipman.notify(#marks == 0 and "No bookmarks in this clip"
      or (dir > 0 and "No bookmark after the playhead" or "No bookmark before the playhead"))
  end
end

return {
  name = "Bookmarks",
  version = "1.1",
  api = 1,
  description = "Timeline bookmarks saved with the clip",

  settings = {
    { key = "before", label = "Seconds before", type = "number", default = 10, min = 0,
      help = "Trim around bookmark: how much to keep before it" },
    { key = "after", label = "Seconds after", type = "number", default = 5, min = 0,
      help = "…and after it" },
  },

  on_clip_opened = function(clip) show(clip) end,

  -- its own "Bookmarks" dropdown in the menu bar, shown in the editor
  menus = { { title = "Bookmarks", screen = "editor", items = {
    { label = "Bookmark here", shortcut = "Ctrl+B",
      run = function(ctx)
        local clip = ctx.clip
        local marks = load(clip.path)
        table.insert(marks, clip.playhead)
        table.sort(marks)
        save(clip.path, marks)
        show(clip)
      end },
    { separator = true },
    { label = "Next bookmark", shortcut = "Alt+ArrowRight",
      run = function(ctx) jump(ctx.clip, 1) end },
    { label = "Previous bookmark", shortcut = "Alt+ArrowLeft",
      run = function(ctx) jump(ctx.clip, -1) end },
    { label = "Trim around nearest bookmark", shortcut = "Ctrl+Shift+B",
      run = function(ctx)
        local clip = ctx.clip
        local marks = load(clip.path)
        local i = nearest(clip, marks)
        if not i then
          clipman.notify("No bookmarks in this clip")
          return
        end
        local s = math.max(0, marks[i] - clipman.settings.before)
        local e = math.min(clip.duration, marks[i] + clipman.settings.after)
        clip:set_range(s, e)
        clip:seek(s)
      end },
    { separator = true },
    { label = "Remove nearest bookmark",
      run = function(ctx)
        local clip = ctx.clip
        local marks = load(clip.path)
        local i = nearest(clip, marks)
        if i then
          table.remove(marks, i)
          save(clip.path, marks)
          show(clip)
        end
      end },
    { label = "Remove all bookmarks",
      run = function(ctx)
        save(ctx.clip.path, {})
        ctx.clip:clear_markers()
      end },
  } } },

  badge = function(path)
    local n = #load(path)
    if n > 0 then return n .. (n == 1 and " bookmark" or " bookmarks") end
  end,
}
