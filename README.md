# Clipman

A native clip browser, trimmer and compressor for game clips. It was made for
GPU Screen Recorder replay clips that need to fit Discord's 20 MB upload limit. It's one
small binary with a native GPU-rendered UI (Rust + [egui](https://github.com/emilk/egui),
no web engine). Encoding goes through ffmpeg via the
[ffmpeg-sidecar](https://github.com/nathanbabcock/ffmpeg-sidecar) wrapper.

## Running

```sh
clipman                                  # clip browser
clipman clip.mp4                         # clip editor
clipman clip.mp4 --preset secondary      # render without the editor
clipman a.mp4 b.mp4 --render             # render several clips with the defaults
clipman --settings                       # just the Settings window
```

Render options (any of them switches to rendering):

| Option | Meaning |
|---|---|
| `--preset ID` | Preset id from Settings (default: the first compress preset) |
| `--start S`, `--end S` | Range to keep, in seconds |
| `--audio 1,2` / `none` | Audio tracks to mix into one (1-based). Default: tracks ticked by default in Settings |
| `--denoise 2,3` / `none` | Tracks to run noise suppression on. Default: tracks set to *On by default* |
| `--out PATH` | Output file (single clip only) |
| `--render` | Render with all the defaults |
| `--no-window` | Show progress in the terminal even when there isn't one attached |

When started from a terminal, renders show progress there. When started from a file
manager, they open a small progress window that closes itself when done. Other commands:
`--write-thunar` (regenerate the Thunar menu) and `--fetch-model` (download the noise
suppression model).

## Clip browser

Shows the clips (`mp4`, `mkv`, `mov`, `webm`) in the folder chosen in Settings (default:
your Videos folder), newest first, with thumbnails. It has a filter box and a Refresh
button. Click a clip to open it in the editor. *Back to clips* returns, and the editor
also returns to the browser after saving.

## Editor

- Drag the **handles under the timeline** to set the start and end. Click or drag the bar
  to seek.
- **Space** plays/pauses (within the selected range). **←/→** step one frame, **Shift+←/→**
  one second. **I** / **O** set the start / end at the playhead. **Ctrl+S** saves.
- Scrubbing is instant: when a clip opens, a small 10 fps preview strip is decoded in the
  background (a few seconds; capped at ~66 MB of memory). While you drag, the nearest strip
  frame shows immediately, and the sharp frame follows once the playhead rests.
- **Audio** dropdown: tick the tracks to keep; they're mixed into one at their original
  volume. Speech tracks have a *Noise suppression* toggle (RNNoise). The preview plays the
  actual mix, with noise suppression once its model is downloaded.
- **Preset** dropdown: every preset from Settings. Compress presets fit the clip under
  their max size (two-pass x264), or use a fixed high quality (CRF 20) when they have no
  max size. *Source* presets copy the video without re-encoding; its start snaps to the
  keyframe at or before the start handle (GSR writes one every ~2 s), and the summary shows
  where it will actually start.
- **Save** renders with a progress bar in the window (Cancel stops it). The editor closes
  when done, or goes back to the browser if it was opened from there.

Output files are `<prefix><clip name><preset suffix>.mp4` next to the clip (prefix default
`remuxed_`). Nothing is ever overwritten; a taken name becomes `…2.mp4`, `…3.mp4`.

## Settings

Stored as JSON in `~/.config/clipman/settings.json` (Windows: `%APPDATA%\clipman\settings.json`).

- **Clips folder** for the browser.
- **Noise suppression strength** 0–100% (blends the original back in; 0% = off).
- **File name prefix**.
- **Quality presets**: name (blank shows as e.g. `720p60`), max height, max fps, max size in
  MB (blank = no limit), file suffix. Add compress or source presets; any preset can
  be removed. Clips are never upscaled.
- **Regenerate right-click menu** (Linux): rewrites the *Clipman* submenu in Thunar from the
  presets (one entry per compress preset, plus *Open in Clipman…*), keeping your other custom
  actions and backing up `uca.xml`. Thunar rereads it after a restart; the dialog offers to
  restart it.
- **Audio tracks**: matched by the name stored in the clip (set in GPU Screen Recorder).
  For each name: how it's shown, whether it's ticked by default, and noise suppression
  (*Not offered* / *Offered (off)* / *On by default*). Unnamed tracks show as "Track N".

## Noise suppression model

RNNoise needs a model file, which isn't shipped. The first time it's used, clipman
downloads `lq.rnnn` ("leavened-quisling", voice vs. general noise, ≈300 KB) from
[GregorR/rnnoise-models](https://github.com/GregorR/rnnoise-models), pinned to a commit and
checked against a SHA-256, and caches it in `~/.cache/clipman/`.

## Building and installing

Needs Rust (stable) and ffmpeg + ffprobe. If ffmpeg isn't found, clipman downloads a build
next to its executable.

**Linux:**

```sh
scripts/install.sh                   # build, install ~/.local/bin/clipman, desktop entry, Thunar menu
scripts/install.sh --hyprland        # also float + center its windows in Hyprland (Lua config)
scripts/install.sh --gsr             # also install the GPU Screen Recorder config in extras/
scripts/install.sh --save-gsr        # copy your live GSR config into extras/
```

Each file is backed up before it's changed, and re-running the script changes nothing more.
The opt-in extras live in `extras/`.

**Arch Linux package:** `packaging/arch/PKGBUILD` builds `clipman-video-git` from the latest
commit on GitHub. It's not called `clipman`, because the AUR's `clipman` / `clipman-git` are an
unrelated Wayland clipboard manager, which also installs `/usr/bin/clipman`, so the two
conflict. The repo is private and makepkg ignores `~/.gitconfig`, so pass it for the GitHub
credentials:

```sh
cd packaging/arch && MAKEPKG_GIT_CONFIG=~/.gitconfig makepkg -si
clipman --write-thunar        # per user, if you want the Thunar right-click entries
```

The package puts the Hyprland rule and the GSR config in `/usr/share/clipman/`.

**Windows:** `cargo build --release` gives `target\release\clipman.exe`. It runs without a
console window and still prints to the terminal when started from one. Thunar integration
is Linux-only; an Explorer context menu isn't implemented yet.

Without network access, build with `--no-default-features`; ffmpeg and the noise model then
have to be installed by hand.

## GPU Screen Recorder config

`extras/gpu-screen-recorder/config_ui` is a snapshot of my GSR UI settings: a 60 s replay
buffer in RAM, 1080p60 at 40 Mbps, saved to `~/Videos`, with three named audio tracks:
**Game** (all apps except Discord, Brave and Feishin), **Voice** (mic) and **Discord**. Your
home directory is stored as `@HOME@`.

## License

Apache License 2.0. See [LICENSE](LICENSE).
