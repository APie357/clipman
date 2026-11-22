-- Float Clipman's windows in the middle of the screen (Hyprland Lua config).
-- scripts/install.sh --hyprland appends this to ~/.config/hypr/hyprland.lua.
hl.window_rule({ name = "clipman", match = { class = "^(clipman)$" }, float = true, center = true })
