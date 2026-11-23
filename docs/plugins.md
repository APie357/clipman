# Plugins

Plugins add features to clipman without changing clipman itself. They're written in
[Lua 5.4](https://www.lua.org/manual/5.4/), which is built into clipman, so there's nothing
extra to install. A plugin can:

- add their own dropdown menus to the menu bar (with keyboard shortcuts) and buttons to
  the editor
- open its own windows, drawn with clipman's UI toolkit
- store metadata on source clips and add it to exported clips
- add ffmpeg filters and options to exports
- react to events: a clip opened, an export started or finished, the browser refreshed
- add markers to the editor's timeline and set the trim range
- declare settings, which get their own tab in **Plugins → Manage plugins…**
- show badges on the browser's clip cards and add right-click actions, also to Thunar's or
  Explorer's menu
- add filters to the browser (e.g. by a tag the plugin stores)

Working examples are in [`examples/plugins`](../examples/plugins):

| Plugin | Shows |
|---|---|
| `game-tags` | a window, sidecar metadata, badges, a browser filter, export metadata and a title tag, settings |
| `bookmarks` | its own dropdown menu, timeline markers, setting the range, shortcuts, `on_clip_opened` |
| `watermark` | an ffmpeg video filter (`drawtext`), choice and number settings |
| `copy-after-export` | `on_export_done`, running a program, a file manager action |

## Installing a plugin

Each plugin is a folder holding an `init.lua`:

```
~/.config/clipman/plugins/<id>/init.lua        (Linux)
%APPDATA%\clipman\plugins\<id>\init.lua        (Windows)
```

The folder name is the plugin's id. It may only contain lowercase letters, digits, `-` and
`_`. **Plugins → Open plugins folder** opens the folder. After adding or editing a plugin,
use **Plugins → Reload plugins**. **Plugins → Manage plugins…** (also a button in Settings)
lists the installed plugins: turn them on or off there, and each plugin with settings gets
its own tab. `clipman --no-plugins` starts without any plugins.

> Plugins run with your permissions, like a shell script or a Neovim config: they get the
> whole Lua standard library (`io`, `os`), and they can run programs. Only install plugins
> you trust.

`require("name")` loads `name.lua` from the plugin's folder, so larger plugins can be split
into modules.

## The plugin table

`init.lua` returns a table. Every field is optional:

```lua
return {
  name = "Game tags",          -- shown in menus and Settings (default: the folder name)
  version = "1.0",
  description = "Tag clips with a game and notes",
  api = 1,                     -- the plugin API it was written for

  settings = { ... },          -- see Settings
  menus = { ... },             -- see Menus
  toolbar = { ... },           -- see Toolbar buttons
  windows = { ... },           -- see Windows
  clip_actions = { ... },      -- see Clip actions
  badge = function(path) ... end,
  browser_filters = { ... },   -- see Browser filters

  on_clip_opened = function(clip, ctx) ... end,
  on_export_start = function(job) ... end,
  on_export_done = function(result) ... end,
  on_browser_refresh = function(paths) ... end,
}
```

### Errors and time limits

A Lua error in a callback **stops the plugin for the rest of the session**. The error shows
in the menu bar and in **Manage plugins**, and is printed to stderr. Fix the plugin, then
use **Plugins → Reload plugins** (or *Reload* in Manage plugins). A plugin that fails to
load is listed in Manage plugins with its error.

Callbacks run on the UI thread, so they must be quick. One that runs longer than 2 seconds
is stopped, or 250 ms for a window's `draw`, `badge` and a filter's `match`. Loading `init.lua` gets
5 seconds. For anything slow, such as uploads or other programs, use
[`clipman.spawn`](#running-programs), which runs in the background and calls you back.

## The `ctx` argument

Menu items, toolbar buttons, clip actions and window `draw` functions receive a `ctx`
table:

| Field | |
|---|---|
| `ctx.screen` | `"editor"`, `"browser"` or `"other"` (file manager actions, command-line renders) |
| `ctx.clip` | the clip open in the editor, or `nil` |

### The clip

`ctx.clip` (and the first argument of `on_clip_opened`) is a snapshot taken when the
callback started:

| Field | |
|---|---|
| `path`, `name` | the clip's full path and file name |
| `duration`, `fps`, `width`, `height` | from ffprobe |
| `audio_names` | the audio tracks' names (`"Track N"` for unnamed ones) |
| `start`, `end` | the trim range, in seconds |
| `playhead` | the playhead position, in seconds |

Methods (applied once the callback returns):

| Method | |
|---|---|
| `clip:add_marker{ t = 12.5, label = "Kill", color = "#ff4040" }` | a marker on the timeline. `label` shows on hover, `color` is optional. Clicking near a marker seeks to it. |
| `clip:clear_markers()` | removes this plugin's markers |
| `clip:set_range(start, end)` | sets the trim range (ignored while exporting) |
| `clip:seek(t)` | moves the playhead |

## Menus

A plugin adds its own dropdowns to the menu bar, between File and Plugins:

```lua
menus = {
  { title = "Upload", screen = "any", items = {
      { label = "Upload clip", shortcut = "Ctrl+U", screen = "editor",
        run = function(ctx) ... end },
      { separator = true },
      { label = "Services", items = {            -- a submenu
          { label = "Streamable", run = function(ctx) ... end },
          { label = "Catbox", run = function(ctx) ... end },
      } },
  } },
}
```

- An entry is an item (`label`, `run`, and optionally `shortcut` and `screen`),
  `{ separator = true }`, or a submenu (`label` plus `items`). Submenus can nest up to 4
  deep.
- `screen` (`"editor"`, `"browser"` or `"any"`, the default) picks where something shows.
  On a dropdown or submenu it applies to everything in it, unless an item sets its own. A
  dropdown or submenu with nothing to show on the current screen is hidden, and so are
  separators left at either end or doubled up.
- `shortcut` uses names like `Ctrl+Shift+B`, `Alt+ArrowRight` or `Ctrl+Comma`. Ctrl is Cmd
  on macOS. A shortcut needs Ctrl or Alt, because plain keys belong to the editor (Space,
  I, O, arrows); the exceptions are F1–F35. Clipman's own shortcuts (Ctrl+O, Ctrl+E,
  Ctrl+S, Ctrl+Shift+E, Ctrl+Comma, Ctrl+Q) win over a plugin's.
- Plugins that use the same `title` share one dropdown, with a separator between them.
- `File`, `Plugins` and `Help` are clipman's own titles; a plugin can't use them. The
  Plugins menu only has *Reload plugins* and *Open plugins folder*.

## Toolbar buttons

```lua
toolbar = {
  { label = "Tags", tooltip = "Edit this clip's tags", run = function(ctx) ... end },
}
```

These are buttons in the editor's bottom row, next to the time display.

## Windows

```lua
windows = {
  { id = "tags", title = "Clip tags", width = 360, height = nil,
    draw = function(ui, ctx) ... end },
}
```

`clipman.open_window("tags")` opens a window and `clipman.close_window("tags")` closes it
(the user can also close it with its ✖). While it's open, `draw` is called every frame.
Like egui, the UI toolkit is *immediate mode*: each frame you describe the whole window, and
widgets return what happened. Keep the window's state in Lua variables, and pass it into the
widgets and back:

```lua
local state = { name = "", loud = false }

draw = function(ui)
  ui:heading("Example")
  state.name = ui:text_edit(state.name, { hint = "Name", width = 200 })
  state.loud = ui:checkbox(state.loud, "Loud")
  if ui:button("Go") then clipman.notify("Hello " .. state.name) end
end
```

### `ui` methods

| Method | Returns |
|---|---|
| `label(text)`, `heading(text)`, `strong(text)`, `weak(text)`, `small(text)`, `monospace(text)` | |
| `error(text)`, `warn(text)`, `colored(text, "#rrggbb")` | |
| `hyperlink(url)`, `hyperlink(text, url)` | |
| `separator()`, `space(px?)` | (in a grid, `space` is an empty cell) |
| `button(text, tooltip?)`, `small_button(text)` | `true` when clicked |
| `checkbox(value, text?)` | the new value |
| `text_edit(value, { hint?, width?, multiline?, password? })` | the new text |
| `slider(value, min, max, text?)` | the new value |
| `drag_value(value, speed?, min?, max?)` | the new value |
| `combo(id, selected, { "a", "b" })` | the selected option |
| `progress(fraction, text?)`, `spinner()` | |
| `horizontal(fn)`, `vertical(fn)`, `collapsing(title, fn)`, `scroll(fn, max_height?)` | |
| `enabled(on, fn)` | (`fn`'s widgets are grayed out when `on` is false) |
| `grid(id, columns?, fn)`, `end_row()` | aligned columns: call `end_row()` after each row |
| `available_width()` | pixels |
| `copy(text)` | copies text to the clipboard |
| `repaint_after(seconds)` | draws again after that long (for animations or polling) |

Container methods call `fn` with a nested `ui`, so draw into that one:
`ui:horizontal(function(ui) ... end)`. A `ui` only works during the call that received it.

## Settings

```lua
settings = {
  { key = "text", label = "Text", default = "", help = "Shown next to the field" },
  { key = "auto", label = "Copy every export", type = "bool", default = true },
  { key = "size", label = "Size", type = "number", default = 4, min = 1, max = 20 },
  { key = "pos", label = "Position", type = "choice", options = { "Top", "Bottom" } },
  { key = "out", label = "Upload folder", type = "folder" },
}
```

The types are `string` (the default), `bool`, `number`, `choice` and `folder`. They show on
the plugin's own tab in **Plugins → Manage plugins…**, and are saved in `settings.json` under
`plugins.<id>.values`. Read them with `clipman.settings.<key>`; missing values fall back to
`default`, or to the first option of a `choice`. New values reach the plugin when the user
clicks OK or Apply.

## Events

| Hook | Called |
|---|---|
| `on_clip_opened(clip, ctx)` | when the editor opens a clip |
| `on_export_start(job)` | before an export starts (editor, command line, file manager). See below. |
| `on_export_done(result)` | after each export: `{ ok, path, size, error, source, job }` (`path` and `size` when `ok`, `error` otherwise) |
| `on_browser_refresh(paths)` | when the browser lists its folder: a list of clip paths |

A window that closes after exporting (an editor opened from the file manager, or a render
progress window) waits until plugins have finished their `clipman.spawn` processes and
closed their windows.

## Adding to exports

`on_export_start(job)` gets:

| Field | |
|---|---|
| `src`, `dst` | source and output paths |
| `kind` | `"compress"` (re-encodes) or `"original"` (a preset without limits: stream copy) |
| `preset` | `{ id, type, name, height, fps, max_mb, suffix }` (`height` / `fps` 0 = the clip's) |
| `start`, `end` | the range in seconds (`nil` = the clip's start / end) |
| `tracks`, `denoise` | 1-based audio track numbers, as in `--audio` |

It returns `nil` (nothing to add) or a table:

| Field | |
|---|---|
| `vf` | a video filter chain, applied after clipman's scaling (compress only) |
| `af` | an audio filter chain, applied after the track mix and noise suppression |
| `output_args` | a list of extra ffmpeg output options, e.g. `{ "-x264-params", "aq-mode=3" }`. They go into every pass. |
| `meta` | any Lua value. It's stored in the export's `clipman` tag as `plugins.<id>`. |
| `tags` | `{ title = "…", artist = "…" }`: plain container tags. `clipman` and `comment` are clipman's own. |
| `cancel` | a reason: the export doesn't start, and the reason is shown |

Notes:

- Filters from several plugins are chained in plugin order (alphabetical by id).
- A preset without limits (*Source*) copies the video without re-encoding, so filters can't apply. A plugin
  that returns `vf` or `af` for an `"original"` job makes the export fail with a message, so
  check `job.kind`.
- Arguments go to ffmpeg as separate arguments, never through a shell. To put text into a
  filter option (such as drawtext's `text=`), escape it with `clipman.filter_escape(text)`.
- `CLIPMAN_DEBUG=1 clipman …` prints every ffmpeg command.

## Metadata

**Source clips**: plugins store data in a sidecar file next to the clip
(`Replay_….mp4.clipman.json`). The recording itself is never rewritten. Each plugin has its
own namespace in it:

```lua
clipman.meta.set(path, { game = "Minecraft" })  -- replaces this plugin's data (nil removes it)
local t = clipman.meta.get(path)           -- this plugin's data, or nil
local all = clipman.meta.get_all(path)     -- every plugin's data, by plugin id
```

The sidecar is deleted once no plugin has data in it. The browser doesn't show sidecars.
Moving a clip outside clipman leaves its sidecar behind.

**Exported clips** carry clipman's `clipman` JSON tag. That's where `meta` from
`on_export_start` ends up, under `plugins`:

```lua
local m = clipman.meta.read_export(path)  -- the whole tag as a table, or nil
print(m.source.name, m.plugins["game-tags"].game)
```

## Clip actions

```lua
clip_actions = {
  { id = "copy", label = "Copy to clipboard", file_manager = true,
    run = function(path, ctx) ... end },
}
```

Clip actions show in the right-click menu of the browser's clip cards. With
`file_manager = true`, they're also added to Thunar's (Linux) or Explorer's (Windows)
*Clipman* submenu the next time it's written: **Settings → Regenerate right-click menu**, or
`clipman --write-menu`. Those entries run `clipman FILE… --plugin-action <plugin>:<id>`,
once per selected file. That opens a small window that shows the plugin's windows, if it
opens any, and closes itself when the plugin is done. From a terminal, or with
`--no-window`, it runs without a window. `id` follows the same rules as plugin ids.

## Badges

```lua
badge = function(path)
  local t = clipman.meta.get(path)
  return t and t.game  -- a short text, or nil for none
end
```

Badges show on the clip's card in the browser. They're computed once per refresh (a few
clips per frame), not every frame, and again after any plugin calls `clipman.meta.set`.

## Browser filters

```lua
browser_filters = {
  { id = "game", label = "Game",
    options = function(paths) return { "Minecraft", "CS2" } end,  -- or a list: { "Minecraft", "CS2" }
    match = function(path, option) return (clipman.meta.get(path) or {}).game == option end },
}
```

Each filter is a dropdown in the browser's top bar, next to the text filter: *Any*, then
the options. When an option is chosen, only clips for which `match(path, option)` returns
true are shown (together with the text filter and other plugins' filters). `options` is
either a list or a function that gets the paths of the listed clips, so it can offer only
what's there. Duplicates and empty strings are dropped.

Like badges, options and matches are worked out when the browser lists its folder and
again after a plugin writes metadata with `clipman.meta.set`, not every frame. `match` is
called for every listed clip at once, so keep it quick (read the sidecar, compare).
A plugin that stops lets every clip through its filters, and its dropdowns go away.

## The `clipman` table

| Function / field | |
|---|---|
| `clipman.version`, `clipman.api` | clipman's version and plugin API version |
| `clipman.plugin_id`, `clipman.plugin_dir` | this plugin's id and folder |
| `clipman.os` | `"linux"`, `"windows"`, `"macos"` |
| `clipman.settings` | this plugin's setting values |
| `clipman.log(...)` | prints to stderr, prefixed with the plugin id |
| `clipman.notify(text)` | shows a message in the menu bar (or the terminal for command-line renders) |
| `clipman.open(path_or_url)` | opens a folder, file or URL with the default app |
| `clipman.open_window(id)`, `clipman.close_window(id)` | |
| `clipman.clips_folder()`, `clipman.export_folder()` | the folders from Settings |
| `clipman.spawn{ ... }` | runs a program in the background (see below) |
| `clipman.meta.get/set/get_all/read_export` | see [Metadata](#metadata) |
| `clipman.json.encode(value, pretty?)`, `clipman.json.decode(text)` | JSON (`null` becomes `nil`) |
| `clipman.filter_escape(text)` | escapes text for an ffmpeg filter option value |

### Running programs

```lua
clipman.spawn{
  args = { "curl", "-sF", "file=@" .. path, "https://example.com/upload" },
  cwd = nil,          -- working directory (optional)
  stdin = nil,        -- text to write to its input (optional)
  on_exit = function(code, stdout, stderr)
    -- on the UI thread once it finishes; code is nil if it couldn't start
  end,
}
```

The program runs directly, without a shell, so arguments need no quoting. On Windows it
runs without a console window. `os.execute` and `io.popen` also work, but they block the
window until the program exits.

## Plugin API versions

`api = 1` is the current version. A plugin that asks for a newer API than this clipman has
isn't loaded, and Manage plugins says to update clipman. New fields and functions can be added
within a version. Changes that break plugins get a new version number.
