# AGENTS.md

Notes for coding agents working on clipman. Read README.md first for what it does.

**Requirements the user set:**

- **One binary** with args. Don't split it into several executables.
- **Native UI only.** No web views, Electron or Tauri; the user finds web apps slow and bloated.
- **ffmpeg through the ffmpeg-sidecar wrapper** (typed builder + event stream), not
  hand-assembled shell strings.
- Render progress in the **editor's own window**; no external progress popups.
- **Cross-platform, Windows planned.** Keep platform-specific bits behind `cfg`.
- Only manage Thunar actions whose unique-id starts with `clipman-`; leave every other entry alone.

## Layout

- `src/main.rs`: CLI (clap) and routing. `clipman` opens the browser, `clipman FILE` the
  editor, and `FILE --render-option` renders (terminal progress when stderr is a TTY or with
  `--no-window`, otherwise the GUI progress window). Paths are made absolute here; see below.
- `src/settings.rs`: the JSON settings. `Preset` (`type` compress|original, blank
  name shows as `<h>p<fps>`, `max_mb` None = no limit), `TrackCfg` keyed by the stream's
  name tag, `output_path()` (single naming scheme, never overwrites, no stacked prefix).
- `src/media.rs`: ffprobe (duration, size, fps, audio name tags, keyframes), `ensure_ffmpeg()`
  (auto-download via ffmpeg-sidecar), and `quiet_command()` (no console window on Windows).
- `src/render.rs`: `render(job, progress, cancel)`. Two-pass x264 to `max_mb × 0.97` with
  retries, CRF 20 when there's no limit, lossless keyframe-aligned trims, the `AudioPlan`
  mix / noise suppression graph, and the RNNoise model download (pinned URL + SHA-256).
- `src/player.rs`: preview. A video thread decodes rgb24 frames (`-vf scale=…`), paced against
  a shared wall clock. Scrubbing: a background "scrub strip" (854 px, 10 fps, max 600 frames,
  stored as JPEG from ffmpeg's MJPEG output) is shown instantly on seek. The exact frame is decoded only after the playhead rests
  (`SEEK_SETTLE`), and a newer command cancels the decode. Every command carries a generation
  number so stale frames are never published. Audio is the same `AudioPlan` decoded to f32 PCM and played through
  rodio. Includes an ignored real-clip test (see Testing).
- `src/thunar.rs`: generates the Thunar `uca.xml` entries (`clipman-preset-<id>`, `clipman-edit`).
- `src/ui/`: eframe app. `mod.rs` (screens + CLI render window), `browser.rs`, `editor.rs`,
  `settings.rs` (modal dialog), `timeline.rs` (custom range widget).
- `packaging/arch/PKGBUILD`: `clipman-video-git` (VCS package; the name avoids the AUR's
  unrelated `clipman` clipboard manager, and it declares `conflicts` with it because both ship
  `/usr/bin/clipman`). Needs `options=('!lto')`: makepkg's LTO makes `ring`'s C objects
  unlinkable by rust-lld. `--remap-path-prefix` keeps `$srcdir` out of the binary. Test with
  `MAKEPKG_GIT_CONFIG=~/.gitconfig makepkg -f` (makepkg ignores the global git config, and the
  repo is private). If dynamic libraries change, re-check `depends` against `ldd` plus the
  dlopened Wayland/X11/GL libs.
- `scripts/install.sh`: Linux installer. `--hyprland` / `--gsr` are opt-in. `extras/` holds
  the Hyprland rule, the desktop entry and the GSR config snapshot (`@HOME@` placeholders).

## Gotchas (all hit during development)

- **ffmpeg runs with the cache dir as its working directory**, so `arnndn=m=lq.rnnn` needs
  no filter-graph escaping (Windows paths like `C:\…` would need it). Consequence: every
  input/output path must be absolute (`std::path::absolute`, not `canonicalize`, which gives
  `\\?\` paths on Windows). `main.rs` and `render()` both do this.
- ffmpeg-sidecar's `.filter()` emits a bare `-filter`, which applies to **all** output
  streams and breaks audio. Use `.args(["-vf", …])` for video filters.
- ffmpeg-sidecar asserts that stdin/stdout/stderr are all piped (it panics otherwise). For raw
  PCM, use `pipe_stdout()` + `take_stdout()` and drain stderr in a thread.
- `arnndn` sometimes emits NaN / huge samples (mostly in its end-of-stream flush). The `aeval`
  step after it zeroes them; without it AAC fails with "Input contains (near) NaN/+-Inf".
  Don't clamp instead: that turns the spikes into clicks.
- `amix=normalize=0` sums tracks like the recorder's own mix. Don't normalize.
- The audio Source runs in the audio callback: never block there (it outputs silence when
  starved, and silence until the shared clock starts).
- Keyboard shortcuts: use `count_and_consume_key`. It uses each event's own modifiers
  (checking `input.modifiers` at frame time misses quick Ctrl+S presses), counts key-repeats,
  and consumes the key so a focused button doesn't also react (Space would toggle twice).
  egui's matching ignores Shift unless asked for, so check `SHIFT+key` **before** the plain key.
- There's no frame at exactly `duration`: an exact seek there decodes nothing. The seek decode
  clamps to `duration - 0.05` and steps back if a decode comes out empty.
- egui's default fonts lack arrows (→ ←); they render as boxes. ✔ ✖ · – … are fine.
- In `egui::Grid`, `TextEdit::desired_width` is ignored; use `ui.add_sized`.
- The editor sizes its window so the video area matches the clip's aspect (`fit_window`,
  first few frames). Don't put a `size` in the Hyprland rule.
- Hyprland is configured in **Lua** (`hl.window_rule{…}`); the window class / app_id is `clipman`.

## Testing

- `cargo test`: unit tests (settings parsing, naming, audio graphs,
  Thunar XML, time parsing).
- Real playback / scrubbing: `CLIPMAN_TEST_CLIP=/path/clip.mp4 cargo test --release -- --ignored --nocapture`
  (`plays_real_clip`: ~60 fps and a correct clock, and it plays a little audio;
  `scrubbing_is_fast`: preview frames instant while dragging, exact frame ~160 ms after resting).
- Renders: copy a clip into a scratch dir and run e.g. `clipman clip.mp4 --preset secondary --start 20 --end 35 --audio 1,2 --denoise 2`
  with `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` pointed at scratch dirs so the user's settings and
  cache aren't touched. `CLIPMAN_DEBUG=1` prints every ffmpeg command and the editor's key events.
  Never write test output into `~/Videos`.
- Driving the GUI on Hyprland: `hyprctl dispatch "hl.dsp.send_shortcut({ mods = 'CTRL', key = 's', window = 'pid:N' })"`
  sends keys. `hyprctl eval 'hl.window_rule{…}'` adds a temporary rule (gone after
  `hyprctl reload`). `grim -g "x,y wxh"` takes screenshots. Don't `pkill -f clipman` from a
  shell whose own command line contains "clipman"; it kills that shell.
- Windows compile check from Linux: `cargo check --target x86_64-pc-windows-gnu --no-default-features`
  (the `download` feature's TLS crate needs MinGW headers to cross-compile; native Windows
  builds are fine).

## Environment

Arch Linux, Hyprland (Lua config), Thunar, GPU Screen Recorder (replay buffer, clips in
`~/Videos` named `Replay_<date>_<time>.mp4`). Python tooling, if ever needed: plain venv + pip,
not uv.
