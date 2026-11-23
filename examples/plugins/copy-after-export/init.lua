-- copy-after-export: put each exported clip on the clipboard, ready to paste into
-- Discord or a browser. Uses wl-copy (Wayland) or xclip (X11) on Linux and PowerShell
-- on Windows.

-- file:// URL with spaces etc. percent-encoded
local function file_url(path)
  return "file://" .. path:gsub("[^%w%-%._~/]", function(c)
    return string.format("%%%02X", c:byte())
  end)
end

local function copy(path, done)
  local args, stdin
  if clipman.os == "windows" then
    -- the path comes in on stdin, so it needs no quoting (Windows PowerShell 5.1)
    args = { "powershell", "-NoProfile", "-Command",
             "Set-Clipboard -Path ([Console]::In.ReadLine())" }
    stdin = path .. "\n"
  elseif os.getenv("WAYLAND_DISPLAY") then
    args = { "wl-copy", "--type", "text/uri-list", file_url(path) }
  else
    args = { "xclip", "-selection", "clipboard", "-t", "text/uri-list" }
    stdin = file_url(path) .. "\n"
  end
  clipman.spawn{
    args = args,
    stdin = stdin,
    on_exit = function(code, _, err)
      if code == 0 then
        clipman.notify("Copied " .. path:match("[^/\\]+$") .. " to the clipboard")
      else
        clipman.notify("Couldn't copy: " .. (err ~= "" and err or tostring(code)))
      end
      if done then done() end
    end,
  }
end

return {
  name = "Copy after export",
  version = "1.0",
  api = 1,
  description = "Puts exported clips on the clipboard",

  settings = {
    { key = "auto", label = "Copy every export", type = "bool", default = true },
  },

  on_export_done = function(r)
    if r.ok and clipman.settings.auto then copy(r.path) end
  end,

  clip_actions = {
    -- also in the file manager's Clipman submenu (after Regenerate right-click menu)
    { id = "copy", label = "Copy to clipboard", file_manager = true,
      run = function(path) copy(path) end },
  },
}
