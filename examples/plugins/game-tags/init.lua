-- game-tags: tag source clips with a game and notes, show the game on the clip
-- browser's cards, and carry the tags into exported clips.
--
-- The tags live in a sidecar next to the clip (Replay_….mp4.clipman.json), so the
-- recording itself is never rewritten.

local editing = nil -- { path = …, game = …, notes = … } while the window is open

local function tags_of(path)
  return clipman.meta.get(path) or {}
end

local function edit(path)
  local t = tags_of(path)
  editing = {
    path = path,
    game = t.game or clipman.settings.default_game,
    notes = t.notes or "",
  }
  clipman.open_window("tags")
end

return {
  name = "Game tags",
  version = "1.0",
  api = 1,
  description = "Tag clips with a game and notes",

  settings = {
    { key = "default_game", label = "Default game", default = "",
      help = "Filled in for clips that have no tag yet" },
    { key = "title_tag", label = "Write a title tag", type = "bool", default = true,
      help = "Exports get a title like \"Apex: clutch 1v3\"" },
  },

  menus = { { title = "Tags", screen = "editor", items = {
    { label = "Tag this clip…", shortcut = "Ctrl+T",
      run = function(ctx) edit(ctx.clip.path) end },
  } } },

  toolbar = {
    { label = "Tags", tooltip = "Game and notes for this clip (Ctrl+T)",
      run = function(ctx) edit(ctx.clip.path) end },
  },

  clip_actions = {
    { id = "edit", label = "Tag clip…", run = function(path) edit(path) end },
  },

  windows = {
    {
      id = "tags", title = "Clip tags", width = 360,
      draw = function(ui)
        if not editing then
          ui:weak("No clip")
          return
        end
        ui:grid("tags", 2, function(ui)
          ui:label("Game")
          editing.game = ui:text_edit(editing.game, { width = 240, hint = "e.g. Apex" })
          ui:end_row()
          ui:label("Notes")
          editing.notes = ui:text_edit(editing.notes, { width = 240, multiline = true })
          ui:end_row()
        end)
        ui:horizontal(function(ui)
          if ui:button("Save") then
            local empty = editing.game == "" and editing.notes == ""
            clipman.meta.set(editing.path, (not empty) and {
              game = editing.game,
              notes = editing.notes,
            } or nil)
            clipman.notify("Saved tags")
            clipman.close_window("tags")
          end
          if ui:button("Cancel") then
            clipman.close_window("tags")
          end
        end)
      end,
    },
  },

  -- shown on the clip's card in the browser
  badge = function(path)
    local g = tags_of(path).game
    if g and g ~= "" then return g end
  end,

  -- exports carry the source's tags (in the clipman tag, as plugins["game-tags"])
  on_export_start = function(job)
    local t = clipman.meta.get(job.src)
    if not t then return nil end
    local edits = { meta = t }
    if clipman.settings.title_tag and (t.game or "") ~= "" then
      local title = t.game
      if (t.notes or "") ~= "" then
        title = title .. ": " .. t.notes:gsub("\n.*", "")
      end
      edits.tags = { title = title }
    end
    return edits
  end,
}
