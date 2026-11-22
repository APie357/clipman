#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install clipman for the current user on Linux (idempotent; safe to re-run).
#  - builds the release binary and copies it to ~/.local/bin/clipman
#  - adds a desktop entry (app launchers, "Open With")
#  - generates the Thunar right-click entries from your presets (if Thunar is installed)
#
# Opt-in extras (combine freely):
#   --hyprland   add the window rule in extras/hyprland/clipman.lua to ~/.config/hypr/hyprland.lua
#   --gsr        install extras/gpu-screen-recorder/config_ui (backs up the current one)
# Other:
#   --save-gsr   only copy the live GPU Screen Recorder config INTO this repo, then exit
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINDIR="${BINDIR:-$HOME/.local/bin}"
EXE="$BINDIR/clipman"
HYPR="$HOME/.config/hypr/hyprland.lua"
GSR="$HOME/.config/gpu-screen-recorder/config_ui"
GSR_REPO="$REPO/extras/gpu-screen-recorder/config_ui"

# The repo copy of the GSR config stores $HOME as @HOME@ (GSR writes JSON-escaped
# slashes, \/home\/me, so substitute in Python rather than sed).
gsr_subst() {
    python3 - "$@" <<'PY'
import os, sys
src, dst, direction = sys.argv[1:]
home = os.path.expanduser("~").replace("/", "\\/")
s = open(src).read()
s = s.replace(home, "@HOME@") if direction == "to-repo" else s.replace("@HOME@", home)
open(dst, "w").write(s)
PY
}

WANT_HYPR=0 WANT_GSR=0
for arg in "$@"; do
    case "$arg" in
        --save-gsr)
            gsr_subst "$GSR" "$GSR_REPO" to-repo
            echo "saved $GSR -> $GSR_REPO"
            exit 0 ;;
        --hyprland) WANT_HYPR=1 ;;
        --gsr) WANT_GSR=1 ;;
        *) echo "usage: $0 [--hyprland] [--gsr] | --save-gsr" >&2; exit 2 ;;
    esac
done

for dep in ffmpeg ffprobe; do
    command -v "$dep" >/dev/null || echo "warning: $dep not found; clipman will offer to download it" >&2
done

# 1. binary
(cd "$REPO" && cargo build --release)
install -Dm755 "$REPO/target/release/clipman" "$EXE"
echo "installed $EXE"

# 2. desktop entry
APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$APPS"
sed "s|@EXE@|$EXE|" "$REPO/extras/clipman.desktop" > "$APPS/clipman.desktop"
ICONS="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
install -Dm644 "$REPO/extras/clipman.png" "$ICONS/256x256/apps/clipman.png"
install -Dm644 "$REPO/extras/clipman.svg" "$ICONS/scalable/apps/clipman.svg"
command -v update-desktop-database >/dev/null && update-desktop-database "$APPS" 2>/dev/null || true
echo "installed $APPS/clipman.desktop"

# 3. Thunar right-click entries (other custom actions are kept; uca.xml is backed up)
if command -v thunar >/dev/null; then
    echo "updated $("$EXE" --write-menu) (restart Thunar to see the menu: thunar -q)"
fi

# 4. Hyprland window rule (opt-in)
if [ "$WANT_HYPR" = 1 ]; then
    if [ ! -f "$HYPR" ]; then
        echo "no $HYPR; see extras/hyprland/clipman.lua"
    elif grep -q 'name = "clipman"' "$HYPR"; then
        echo "Hyprland rule already present"
    else
        cp "$HYPR" "$HYPR.bak"
        { echo; grep -v '^--' "$REPO/extras/hyprland/clipman.lua"; } >> "$HYPR"
        echo "added Hyprland rule to $HYPR (backup: $HYPR.bak)"
        command -v hyprctl >/dev/null && hyprctl reload >/dev/null && hyprctl configerrors
    fi
fi

# 5. GPU Screen Recorder config (opt-in)
if [ "$WANT_GSR" = 1 ]; then
    mkdir -p "$(dirname "$GSR")"
    [ -f "$GSR" ] && cp "$GSR" "$GSR.bak"
    gsr_subst "$GSR_REPO" "$GSR" from-repo
    echo "installed $GSR (backup: $GSR.bak); restart gpu-screen-recorder-ui to load it"
fi
echo done
