#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build Clipman-<version>-<arch>.AppImage into target/appimage/.
#
# Uses linuxdeploy (downloaded into target/tools on first run), which bundles the shared
# libraries the binary links against except the ones every system provides (glibc, GL,
# X11/Wayland, ...). ffmpeg isn't bundled: clipman uses the system's, or downloads one
# into ~/.local/share/clipman/ffmpeg on first run.
#
# Build on the oldest distro you want to support (CI uses Ubuntu 22.04): the AppImage
# needs a glibc at least as new as the build machine's.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
ARCH="${ARCH:-$(uname -m)}"
OUT="$REPO/target/appimage"
APPDIR="$OUT/AppDir"
TOOLS="$REPO/target/tools"
LINUXDEPLOY="$TOOLS/linuxdeploy-$ARCH.AppImage"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

cargo build --release --locked

if [ ! -x "$LINUXDEPLOY" ]; then
    mkdir -p "$TOOLS"
    curl -fL --retry 3 -o "$LINUXDEPLOY" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
    chmod +x "$LINUXDEPLOY"
fi

rm -rf "$APPDIR"
mkdir -p "$OUT"
sed 's|@EXE@|clipman|' extras/clipman.desktop > "$OUT/clipman.desktop"

# no FUSE needed (CI containers, some desktops)
export APPIMAGE_EXTRACT_AND_RUN=1
export LINUXDEPLOY_OUTPUT_VERSION="$VERSION"
(
    cd "$OUT"
    "$LINUXDEPLOY" \
        --appdir "$APPDIR" \
        --executable "$REPO/target/release/clipman" \
        --desktop-file "$OUT/clipman.desktop" \
        --icon-file "$REPO/extras/clipman.png" \
        --output appimage
)
ls -1 "$OUT"/*.AppImage
