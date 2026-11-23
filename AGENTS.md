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
- `src/settings.rs`: the JSON settings. `Preset` (`height` / `fps` 0 = keep the
  clip's, `max_mb` None = no limit; `kind()` follows from them: no limit at all = original,
  a stream copy. `type` is still written to the file (`PresetFile`) and an old "original"
  entry loads with no limits; blank name shows as e.g. `720p60`), `TrackCfg` keyed by the stream's
  name tag, `output_path()` (single naming scheme into `export_folder()`, default
  `<Videos>/Exports`, never overwrites (`_2`, `_3`), no stacked prefix). The browser isn't
  recursive, which is what keeps exports out of it.
- `src/media.rs`: ffprobe (duration, size, fps, audio name tags, frame/keyframe times),
  `ffmpeg_bin()` / `ffprobe_bin()` (next to the exe, then PATH, then the download dir) and
  `ensure_ffmpeg()`, which downloads into the per-user data dir because the exe dir can be
  read-only (AppImage, Program Files). Always use these instead of ffmpeg-sidecar's own path
  helpers. `quiet_command()` avoids console windows on Windows.
- `src/render.rs`: `render(job, progress, cancel)`. Two-pass x264 to `max_mb × 0.97` with
  retries, CRF 20 when there's no limit, lossless keyframe-aligned trims, the `AudioPlan`
  mix / noise suppression graph, and the RNNoise model download (pinned URL + SHA-256).
- `src/gpu.rs`: hardware H.264 (VAAPI on Linux, render nodes tried biggest-VRAM first so
  a dGPU beats the iGPU; NVENC; AMF / QSV on Windows). `detect()` test-encodes a few frames
  per candidate once per process. The `Encoder` setting (`auto` = GPU only without a size
  limit) decides per job; `render_compress` falls back to x264 if the GPU encode errors.
  Hardware has no 2-pass: size-limited GPU renders are one VBR pass plus the size retries.
  Measured on the RX 7900 (30 s 1440p60 to 720p60): VAAPI ~4.5x faster than x264 slow, VMAF
  89 vs 95 at the same size; QP 24 ~ CRF 20 quality at ~1.5x the size. Only VAAPI has been
  tested on real hardware; the NVENC/AMF/QSV options are untested.
- `src/player.rs`: preview. A video thread decodes rgb24 frames (`-vf scale=…`), paced against
  a shared wall clock. Scrubbing: a background "scrub strip" (854 px, 10 fps, max 600 frames,
  stored as JPEG from ffmpeg's MJPEG output) is shown instantly on seek. The exact frame is decoded only after the playhead rests
  (`SEEK_SETTLE`), and a newer command cancels the decode. Every command carries a generation
  number so stale frames are never published. Frame stepping (`step_to`) moves between real
  frame timestamps (`media::frame_times`, from packet pts, so variable frame rate works too)
  and never shows the strip. Exact seeks decode a window of frames (3 before, 9 after) and
  keep it, so nearby steps are instant. Seek times are formatted with 6 decimals: millisecond
  rounding (0.0167 → 0.017) skipped past frames. Audio is the same `AudioPlan` decoded to f32 PCM and played through
  rodio. Includes an ignored real-clip test (see Testing). The `gpu_playback` setting adds
  `-hwaccel auto` to playback only (falls back to software by itself). Measured on the RX 7900
  with 1440p60: its decoder does ~700 fps vs ~1000 fps for software on all cores, and each GPU
  ffmpeg costs ~70 ms of setup, so seeks and the strip are slower on the GPU (the strip and a
  seek also contend for the one decoder); playback halves CPU use but starts ~80 ms later.
  AMD has no JPEG encoder, so the strip can't stay on the GPU.
- `src/plugin/`: Lua 5.4 plugins (mlua, vendored; `plugins` cargo feature, default on;
  `stub.rs` is a no-op `Host` with the same API for `--no-default-features`). `host.rs`
  loads `<config>/plugins/<id>/init.lua` (one `Lua` per plugin, full stdlib by the user's
  choice: plugins are trusted like a config file), installs the `clipman` table, and calls
  into plugins through `Plugin::guard`: an error or a callback over its time budget (an
  instruction-count hook) stops that plugin for the session. Lua calls queue requests
  (`Shared`: clip commands, windows, `spawn`s, notices), which the host handles after the
  call. `ui.rs` wraps `&mut egui::Ui` as scoped userdata (value in, value out widgets).
  `meta.rs` is the source-clip sidecar `<clip>.clipman.json`. `docs/plugins.md` is the API
  reference; `examples/plugins` are tested by `examples_load`. Plugin clip actions with
  `file_manager = true` become `clipman-plugin-<plugin>-<id>` Thunar entries and numbered
  Explorer subkeys running `--plugin-action`. Badges and `browser_filters` are cached by
  the browser and asked for again when `Host::changes()` moves (plugins loaded, stopped or
  given new settings, or any `clipman.meta.set`).
- `src/update.rs`: update checker (`update-check` cargo feature, default on; the PKGBUILD
  builds without it). GUI windows only, never CLI renders: one GitHub `releases/latest`
  request a day, cached with the dismissed version in `<cache>/update-check.json`. The UI
  only links to the release page. To see the UI without a release, write a fresh
  `checked_at` and a `latest` of 9.9.9 into that file (scratch `XDG_CACHE_HOME`).
- `src/menu.rs`: right-click menu entry points (`--write-menu`, `--remove-menu`, the Settings
  button), dispatching to `thunar.rs` (Linux) or `explorer.rs` (Windows).
- `src/thunar.rs`: generates the Thunar `uca.xml` entries (`clipman-preset-<id>`, `clipman-edit`).
- `src/explorer.rs`: Explorer entries in HKCU (`…\SystemFileAssociations\<ext>\shell\clipman`,
  a `SubCommands` cascade with numbered subkeys because Explorer sorts them by name). Only
  that `clipman` key is ever written or deleted.
- `src/ui/`: eframe app. `mod.rs` (screens, the File/Help menu bar and its shortcuts
  (`MenuCmd`), the CLI render window), `browser.rs` (cards laid out in centered rows by
  hand, width from the `card_size` setting / top-bar slider, 160 to 480; thumbnails are
  cached 512 px wide, so no regeneration on resize), `editor.rs`,
  `settings.rs` (modal dialog), `plugins.rs` (the Plugins dialog: an Installed tab to turn
  plugins on/off plus a settings tab per plugin; opened from the Plugins menu or Settings'
  *Manage plugins…*, drawn over Settings, saves only `settings.plugins` and keeps Settings'
  draft in sync via `set_plugins`), `timeline.rs` (custom range widget).
- `packaging/arch/PKGBUILD`: `clipman-video-git` (VCS package; the name avoids the AUR's
  unrelated `clipman` clipboard manager, and it declares `conflicts` with it because both ship
  `/usr/bin/clipman`). Needs `options=('!lto')`: makepkg's LTO makes `ring`'s C objects
  unlinkable by rust-lld. `--remap-path-prefix` keeps `$srcdir` out of the binary. Test with
  `makepkg -f` (it builds the latest commit pushed to GitHub, not the working tree). If
  dynamic libraries change, re-check `depends` against `ldd` plus the dlopened
  Wayland/X11/GL libs.
- `packaging/appimage/build-appimage.sh`: AppImage via linuxdeploy (downloaded to
  `target/tools`, run with `APPIMAGE_EXTRACT_AND_RUN=1` so no FUSE is needed). ffmpeg isn't
  bundled. The Thunar menu runs `$APPIMAGE` (`menu::self_exe`): the mount path changes.
- `packaging/flatpak/`: manifest (`men.goobers.clipman`, Freedesktop 26.08 + rust-stable,
  cargo fetches with `--share=network`, so not Flathub-ready), metainfo and
  `build-flatpak.sh` (bundle in `target/flatpak/`; uses `flatpak-builder` or the
  `org.flatpak.Builder` app). The Wayland app_id stays `clipman` (`StartupWMClass`). In the
  sandbox (`FLATPAK_ID` set) the Thunar menu goes to the host's config
  (`HOST_XDG_CONFIG_HOME`) and runs `flatpak run <id>`; config and cache are under
  `~/.var/app/<id>/`. ffmpeg is the runtime's (`/usr/bin/ffmpeg`, full codecs via
  codecs-extra, VAAPI works with `--device=dri`). Flatpak sets `HOST_XDG_CONFIG_HOME` itself
  (`--env` can't override it), so testing `--write-menu` in the sandbox writes the real
  `~/.config/Thunar/uca.xml` (restore it from `uca.xml.bak`). CI builds it in the `flatpak`
  job (flatpak-github-actions container) and the release attaches the bundle. The SDK's
  newer Rust warns where `Stroke::new(1.0, …)` literals fall back to f32; write `1.0_f32`.
- `.github/workflows/build.yml`: Linux (ubuntu-22.04 for an old glibc baseline: tests, the
  AppImage and a tarball) and Windows (tests, a zipped exe) on push/PR; `v*` tags also publish
  a release. A `check` job makes a branch push skip its build when a `v*` tag points at the
  same commit (the tag run builds and releases it), so release with
  `git push --atomic origin main vX.Y.Z` (both refs at once, so the tag exists when the check
  runs). Bump `version` in Cargo.toml first, or the update checker offers the release to
  itself. The repo is public, so Actions minutes are free. Workflows from forks' pull
  requests get a read-only token; only the tag-triggered release job can write.
- `install.sh`: per-user Linux install (binary, desktop entry, icons, Thunar menu).
  `extras/` holds the desktop entry template (`@EXE@`) and the icons.

## Gotchas (all hit during development)

- **Lua only runs on the UI thread** (or main, for CLI renders). Plugin additions reach a
  render only through `Job.extra` (`render::Extra`), filled by `Host::export_start` before
  the job starts, so `render()` never touches Lua. Keep it that way: mlua's `Lua` is `!Send`
  here (no `send` feature). The CLI render path loads plugins too, so a Thunar render gets
  the same filters and metadata as the editor.
- Plugin `vf` goes after clipman's scale but before the GPU encoder's `hwupload` suffix
  (software filters first). Text in a filter option needs two escaping levels (option, then
  graph); `plugin::filter_escape` does both and is checked against real drawtext. Quoting
  with `'…'` breaks on apostrophes.
- Windows that close after exporting (`close_when_idle` in `ui/mod.rs`) wait for plugin
  processes and windows, or `on_export_done` uploads would die with the process.
- Several shortcuts can land in one frame (scripted input does this): `plugin_shortcuts`
  returns every match, not the first.

- **Exported clips carry metadata** (`Meta` in `render.rs`): a `clipman` JSON tag plus
  `comment`. mp4 drops custom keys unless `-movflags +use_metadata_tags`; that mode also
  duplicates the source's global tags, hence `-map_metadata:g -1`. Store the source's file
  name only, never its path (clips get shared). Bump `format` if the JSON changes meaning.
- **Frame-exact cuts:** GSR timestamps sit a hair off the grid (4.999999, 7.999999), so
  cutting at nominal times kept an extra frame. `exact_span` cuts from just before the
  first kept frame's real pts to just before the first dropped one, formatted with 6
  decimals. With stream copy, `-t` counts from the seek point (keyframe + 1 ms), not from the
  keyframe. The metadata's `start_frame` / `end_frame` use the same `frame_at` rule, and the
  file's frame count matches `end_frame - start_frame`.
- **ffmpeg runs with the cache dir as its working directory**, so `arnndn=m=lq.rnnn` needs
  no filter-graph escaping (Windows paths like `C:\…` would need it). Consequence: every
  input/output path must be absolute (`std::path::absolute`, not `canonicalize`, which gives
  `\\?\` paths on Windows). `main.rs` and `render()` both do this.
- ffmpeg-sidecar's `.filter()` emits a bare `-filter`, which applies to **all** output
  streams and breaks audio. Use `.args(["-vf", …])` for video filters.
- ffmpeg-sidecar asserts that stdin/stdout/stderr are all piped (it panics otherwise). For raw
  PCM, use `pipe_stdout()` + `take_stdout()` and drain stderr in a thread.
- **No vsync** (`vsync: false` in `ui::run`): with it, presenting waits for a Wayland frame
  callback, which a window on a hidden workspace never gets, so the UI thread blocks and
  Hyprland shows "not responding". Without vsync's cap, a repaint loop runs flat out, and
  egui subtracts the predicted frame time (1/60 s) from `request_repaint_after`: a 16 ms
  delay means "now" (8000+ fps). Use 33 ms or more for timers.
- **Never call an `rfd` dialog on the UI thread.** It blocks drawing until the dialog closes
  and Hyprland then reports clipman as not responding. Use `ui::PendingDialog::spawn` (runs
  it on a thread, poll the result each frame), as Open, Export As and the Settings folder
  pickers do.
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
- `ui.add_space` inside an `egui::Grid` is a **debug-build panic** (release builds skip the
  assertion, so release-only testing misses it). Wrap it in `ui.horizontal`. The headless
  UI tests (`draws_plugin_settings`, `draws_every_widget`) draw with `egui::Context::run` in
  debug builds to catch these.
- The editor sizes its window so the video area matches the clip's aspect (`fit_window`,
  first few frames). Don't put a `size` in a Hyprland window rule for it.
- Hyprland is configured in **Lua** (`hl.window_rule{…}`); the window class / app_id is `clipman`.

## Testing

- `cargo test`: unit tests (settings parsing, naming, audio graphs,
  Thunar XML, time parsing).
- Real playback / scrubbing: `CLIPMAN_TEST_CLIP=/path/clip.mp4 cargo test --release -- --ignored --nocapture`
  (`plays_real_clip`: ~60 fps and a correct clock, and it plays a little audio; add
  `CLIPMAN_TEST_GPU=1` to play with GPU decoding;
  `scrubbing_is_fast`: preview frames instant while dragging, exact frame ~160 ms after resting).
- Renders: copy a clip into a scratch dir and run e.g. `clipman clip.mp4 --preset secondary --start 20 --end 35 --audio 1,2 --denoise 2`
  with `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` pointed at scratch dirs so the user's settings and
  cache aren't touched. `CLIPMAN_DEBUG=1` prints every ffmpeg command and the editor's key events.
  Never write test output into `~/Videos`; the default export folder is `~/Videos/Exports`, so
  also point `HOME` at a scratch dir (or set `export_folder`) when testing renders.
- Driving the GUI on Hyprland: `hyprctl dispatch "hl.dsp.send_shortcut({ mods = 'CTRL', key = 'e', window = 'pid:N' })"`
  sends keys. For clicks (menus, file dialogs) and typing, the user is in the `plover` group,
  so `/dev/uinput` is writable and python-evdev is installed: a throwaway uinput mouse,
  moved with small relative steps checked against `hyprctl cursorpos` (acceleration makes
  single jumps miss), then `BTN_LEFT`. Click into a portal dialog's field before typing; it
  doesn't take focus on its own. `hyprctl eval 'hl.window_rule{…}'` adds a temporary rule (gone after
  `hyprctl reload`). `grim -g "x,y wxh"` takes screenshots. Don't `pkill -f clipman` from a
  shell whose own command line contains "clipman"; it kills that shell.
- Screenshots without touching the user's screens: `hyprctl output create headless NAME`,
  then move the window there with
  `hyprctl dispatch "hl.dsp.window.move({ workspace = N, follow = false, window = 'pid:P' })"`,
  then `grim -o NAME`. `hyprctl eval 'hl.monitor({ output = "NAME", mode = "1600x2400@60", position = "5120x0", scale = 1 })'`
  makes it taller (egui remembers a modal's size, so reopen the dialog in a fresh process
  afterwards). Clean up with `hyprctl output remove NAME`.
- Windows compile check from Linux: `cargo check --target x86_64-pc-windows-gnu --no-default-features`
  (the `download` feature's TLS crate needs MinGW headers to cross-compile; native Windows
  builds are fine).
- Running the Windows build under Wine: there's no MinGW toolchain installed, but zig can link.
  Use a linker shim that runs `zig cc -target x86_64-windows-gnu`, drops `-lmsvcrt`,
  `-lmingwex`, `-lmingw32`, `-lgcc`, `-l:libpthread.a`, `-nodefaultlibs`, `-Wl,-B*`,
  replaces `-lgcc_eh` with `-lunwind` and `-lwindows.0.52.0` with the `.a` path in the
  `windows_x86_64_gnu` crate. Also put an `x86_64-w64-mingw32-dlltool` on PATH that calls
  `llvm-dlltool` without `-f`, `--temp-prefix`, `--no-leading-underscore` or `--64`. Set
  `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER` / `CC_x86_64_pc_windows_gnu` to the shim, then
  `cargo build --target x86_64-pc-windows-gnu --no-default-features`. Use a scratch
  `WINEPREFIX`; `wine reg export` (UTF-16) shows non-ASCII values correctly, `reg query` doesn't.

## Environment

Arch Linux, Hyprland (Lua config), Thunar, GPU Screen Recorder (replay buffer, clips in
`~/Videos` named `Replay_<date>_<time>.mp4`). Python tooling, if ever needed: plain venv + pip,
not uv.
