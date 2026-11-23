#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build Clipman-<version>-<arch>.flatpak (a single-file bundle) into target/flatpak/.
#
# Needs flatpak-builder (the distro package, or `flatpak install flathub org.flatpak.Builder`)
# and the Flathub remote; the SDK and its Rust extension are installed per user if missing.
# Install the result with `flatpak install --user target/flatpak/Clipman-*.flatpak`.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"
APP=men.goobers.clipman
ARCH="${ARCH:-$(uname -m)}"
OUT="$REPO/target/flatpak"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

if command -v flatpak-builder >/dev/null; then
    BUILDER=(flatpak-builder)
else
    BUILDER=(flatpak run --filesystem="$REPO" org.flatpak.Builder)
fi

mkdir -p "$OUT"
"${BUILDER[@]}" --user --install-deps-from=flathub --force-clean --arch="$ARCH" \
    --state-dir="$OUT/state" --repo="$OUT/repo" "$OUT/build" \
    "packaging/flatpak/$APP.yml"
# --runtime-repo: installing the bundle fetches the runtime from Flathub if it's missing
flatpak build-bundle --arch="$ARCH" --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$OUT/repo" "$OUT/Clipman-$VERSION-$ARCH.flatpak" "$APP"
ls -1 "$OUT"/*.flatpak
