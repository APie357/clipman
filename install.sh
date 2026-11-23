#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install clipman for the current user on Linux (idempotent; safe to re-run).
#  - builds the release binary and copies it to ~/.local/bin/clipman
#  - adds a desktop entry (app launchers, "Open With")
#  - generates the Thunar right-click entries from your presets (if Thunar is installed)
set -euo pipefail

[ $# -eq 0 ] || { echo "usage: $0" >&2; exit 2; }

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINDIR="${BINDIR:-$HOME/.local/bin}"
EXE="$BINDIR/clipman"

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

echo done
