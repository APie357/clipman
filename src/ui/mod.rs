// SPDX-License-Identifier: Apache-2.0
//! The native window: clip browser, clip editor, and a progress window for renders
//! started from the command line / right-click menu.

mod browser;
mod editor;
mod settings;
mod timeline;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, Vec2};

use crate::render::{self, Job};
use crate::settings::Settings;
use settings::{Outcome, SettingsUi};

pub const APP_ID: &str = "clipman";

/// Height of every button, dropdown and text field, so controls line up.
pub const CONTROL_HEIGHT: f32 = 26.0;

fn apply_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.interact_size.y = CONTROL_HEIGHT;
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        style.spacing.item_spacing.x = 6.0;
    });
}

/// Commands from the menu bar (or their keyboard shortcuts).
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuCmd {
    Open,
    Export,
    ExportAs,
    OpenExportFolder,
    Back,
    Settings,
    Quit,
}

const SC_OPEN: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
const SC_EXPORT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::E);
const SC_EXPORT_AS: egui::KeyboardShortcut = egui::KeyboardShortcut::new(
    egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
    egui::Key::E,
);
const SC_SETTINGS: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Comma);
const SC_QUIT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Q);

/// A native file dialog running on its own thread. Calling it on the UI thread blocks
/// drawing until it closes, and compositors then report the app as not responding.
pub struct PendingDialog(Receiver<Option<PathBuf>>);

impl PendingDialog {
    pub fn spawn(
        ctx: &egui::Context,
        dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(dialog());
            ctx.request_repaint();
        });
        Self(rx)
    }

    /// Some(the chosen path, or None if cancelled) once the dialog has closed.
    pub fn poll(&self) -> Option<Option<PathBuf>> {
        match self.0.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => Some(None),
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

/// Which menu dialog is open.
enum MenuDialog {
    Open,
    ExportAs,
}

/// Video files the Open dialog offers (same as the clip browser).
pub const VIDEO_EXTENSIONS: [&str; 4] = ["mp4", "mkv", "mov", "webm"];

pub enum Start {
    Browser,
    Editor(PathBuf),
    Render(Vec<Job>),
    Settings,
}

enum Screen {
    Browser,
    Editor(Box<editor::Editor>),
    Render(RenderWindow),
    /// Only the settings dialog (`clipman --settings`).
    SettingsOnly,
    Error(String),
}

pub struct App {
    settings: Settings,
    screen: Screen,
    browser: Option<browser::Browser>,
    settings_ui: Option<SettingsUi>,
    audio: Option<rodio::MixerDeviceSink>,
    audio_tried: bool,
    /// An Open / Export As dialog that's open.
    dialog: Option<(MenuDialog, PendingDialog)>,
}

pub fn initial_size(start: &Start) -> Vec2 {
    match start {
        Start::Browser => Vec2::new(1180.0, 780.0),
        Start::Editor(p) => crate::media::probe(p)
            .map(|i| editor::Editor::initial_window_size(&i))
            .unwrap_or(Vec2::new(1296.0, 870.0)),
        Start::Render(_) => Vec2::new(560.0, 220.0),
        Start::Settings => Vec2::new(920.0, 760.0),
    }
}

fn icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../../extras/clipman.png"))
        .expect("bundled icon")
        .into_rgba8();
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}

pub fn run(start: Start) -> eframe::Result<()> {
    let title = match &start {
        Start::Editor(p) => format!(
            "Clipman — {}",
            p.file_name().unwrap_or_default().to_string_lossy()
        ),
        Start::Render(_) => "Clipman — rendering".into(),
        _ => "Clipman".into(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID) // Wayland app_id / X11 class, e.g. for Hyprland rules
            .with_title(title)
            .with_inner_size(initial_size(&start))
            .with_min_inner_size([480.0, 200.0])
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native(
        "clipman",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(start, &cc.egui_ctx)))),
    )
}

impl App {
    fn new(start: Start, ctx: &egui::Context) -> Self {
        apply_style(ctx);
        let settings = Settings::load();
        let mut app = Self {
            settings,
            screen: Screen::Browser,
            browser: None,
            settings_ui: None,
            audio: None,
            audio_tried: false,
            dialog: None,
        };
        if !matches!(start, Start::Render(_)) {
            crate::update::start(app.settings.check_updates);
        }
        app.screen = match start {
            Start::Browser => {
                app.browser = Some(browser::Browser::new(&app.settings));
                Screen::Browser
            }
            Start::Editor(path) => app.open_editor(path, ctx, false),
            Start::Render(jobs) => Screen::Render(RenderWindow::start(jobs)),
            Start::Settings => {
                app.settings_ui = Some(SettingsUi::new(&app.settings, &[]));
                Screen::SettingsOnly
            }
        };
        app
    }

    fn mixer(&mut self) -> Option<&rodio::mixer::Mixer> {
        if !self.audio_tried {
            self.audio_tried = true;
            self.audio = crate::player::open_audio();
        }
        self.audio.as_ref().map(|s| s.mixer())
    }

    fn open_editor(&mut self, path: PathBuf, ctx: &egui::Context, from_browser: bool) -> Screen {
        match editor::Editor::new(path.clone(), &self.settings, ctx, from_browser) {
            Ok(ed) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("Clipman — {name}")));
                if from_browser {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                        editor::Editor::initial_window_size(&ed.info),
                    ));
                }
                Screen::Editor(Box::new(ed))
            }
            Err(e) => Screen::Error(format!("Couldn't open {}:\n{e:#}", path.display())),
        }
    }

    /// The File / Help menu bar shown above the browser and the editor.
    fn menu_bar(&self, ctx: &egui::Context) -> Option<MenuCmd> {
        let editor = match &self.screen {
            Screen::Editor(ed) => Some(ed),
            _ => None,
        };
        // nothing that changes the clip while an export or a file dialog is running
        let busy = editor.is_some_and(|e| e.is_exporting()) || self.dialog.is_some();
        let can_export = !busy && editor.is_some_and(|e| e.can_export(&self.settings));
        let mut cmd = None;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let item = |ui: &mut egui::Ui, enabled: bool, text: &str, sc: Option<egui::KeyboardShortcut>| {
                    let mut b = egui::Button::new(text);
                    if let Some(sc) = sc {
                        // "Ctrl+Comma" reads better as "Ctrl+,"
                        b = b.shortcut_text(ui.ctx().format_shortcut(&sc).replace("Comma", ","));
                    }
                    ui.add_enabled(enabled, b).clicked()
                };
                ui.menu_button("File", |ui| {
                    if item(ui, !busy, "Open clip\u{2026}", Some(SC_OPEN)) {
                        cmd = Some(MenuCmd::Open);
                    }
                    if editor.is_some() {
                        ui.separator();
                        if item(ui, can_export, "Export", Some(SC_EXPORT)) {
                            cmd = Some(MenuCmd::Export);
                        }
                        if item(ui, can_export, "Export As\u{2026}", Some(SC_EXPORT_AS)) {
                            cmd = Some(MenuCmd::ExportAs);
                        }
                    }
                    if item(ui, true, "Open export folder", None) {
                        cmd = Some(MenuCmd::OpenExportFolder);
                    }
                    if editor.is_some_and(|e| e.from_browser) {
                        ui.separator();
                        if item(ui, !busy, "Back to clip browser", None) {
                            cmd = Some(MenuCmd::Back);
                        }
                    }
                    ui.separator();
                    if item(ui, true, "Settings\u{2026}", Some(SC_SETTINGS)) {
                        cmd = Some(MenuCmd::Settings);
                    }
                    ui.separator();
                    if item(ui, true, "Quit", Some(SC_QUIT)) {
                        cmd = Some(MenuCmd::Quit);
                    }
                });
                ui.menu_button("Help", |ui| {
                    ui.label(format!("Clipman {}", crate::update::CURRENT));
                    if let crate::update::Status::Available(r) = crate::update::status() {
                        ui.hyperlink_to(format!("Clipman {} is available", r.version), &r.url);
                    }
                    ui.hyperlink_to(
                        "Project page",
                        format!("https://github.com/{}", crate::update::REPO),
                    );
                });
            });
        });
        cmd
    }

    /// Menu shortcuts, checked every frame (also when the menu is closed).
    fn menu_shortcuts(&self, ctx: &egui::Context) -> Option<MenuCmd> {
        if self.settings_ui.is_some() || self.dialog.is_some() {
            return None; // a dialog has the keyboard
        }
        use egui::{Key, Modifiers as M};
        ctx.input_mut(|inp| {
            // each event's own modifiers (a quick Ctrl+E can release Ctrl before the frame
            // is drawn); Shift first, since the plain Ctrl+E check would also match it
            let mut n = |m, k| inp.count_and_consume_key(m, k) > 0;
            if n(M::COMMAND | M::SHIFT, Key::E) {
                Some(MenuCmd::ExportAs)
            } else if n(M::COMMAND, Key::E) || n(M::COMMAND, Key::S) {
                Some(MenuCmd::Export) // Ctrl+S: what Save used to be
            } else if n(M::COMMAND, Key::O) {
                Some(MenuCmd::Open)
            } else if n(M::COMMAND, Key::Comma) {
                Some(MenuCmd::Settings)
            } else if n(M::COMMAND, Key::Q) {
                Some(MenuCmd::Quit)
            } else {
                None
            }
        })
    }

    fn run_menu(&mut self, cmd: MenuCmd, ctx: &egui::Context) {
        match cmd {
            MenuCmd::Open => {
                if matches!(&self.screen, Screen::Editor(ed) if ed.is_exporting()) {
                    return;
                }
                if self.dialog.is_some() {
                    return;
                }
                if let Screen::Editor(ed) = &mut self.screen {
                    ed.pause();
                }
                let dir = self.settings.clips_folder();
                let d = PendingDialog::spawn(ctx, move || {
                    rfd::FileDialog::new()
                        .set_title("Open clip")
                        .set_directory(dir)
                        .add_filter("Video", &VIDEO_EXTENSIONS)
                        .pick_file()
                });
                self.dialog = Some((MenuDialog::Open, d));
            }
            MenuCmd::Export => {
                if let Screen::Editor(ed) = &mut self.screen {
                    ed.export(&self.settings, None);
                }
            }
            MenuCmd::ExportAs => {
                let Screen::Editor(ed) = &mut self.screen else {
                    return;
                };
                let Some(default) = ed.default_export_path(&self.settings) else {
                    return;
                };
                if self.dialog.is_some() || !ed.can_export(&self.settings) {
                    return;
                }
                ed.pause();
                // start in the export folder (the picker needs it to exist)
                let mut dir = default.parent().map(PathBuf::from).unwrap_or_default();
                if std::fs::create_dir_all(&dir).is_err() {
                    dir = self.settings.clips_folder();
                }
                let name = default.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let d = PendingDialog::spawn(ctx, move || {
                    rfd::FileDialog::new()
                        .set_title("Export as")
                        .set_directory(dir)
                        .set_file_name(name)
                        .add_filter("MP4 video", &["mp4"])
                        .save_file()
                });
                self.dialog = Some((MenuDialog::ExportAs, d));
            }
            MenuCmd::OpenExportFolder => {
                let dir = self.settings.export_folder();
                let _ = std::fs::create_dir_all(&dir);
                open_folder(&dir);
            }
            MenuCmd::Back => {
                if let Screen::Editor(ed) = &mut self.screen
                    && !ed.is_exporting()
                {
                    ed.stop();
                    self.back_to_browser(ctx);
                }
            }
            MenuCmd::Settings => {
                if self.settings_ui.is_none() {
                    let tracks = match &self.screen {
                        Screen::Editor(ed) => ed.track_names().to_vec(),
                        _ => Vec::new(),
                    };
                    self.settings_ui = Some(SettingsUi::new(&self.settings, &tracks));
                }
            }
            MenuCmd::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    /// Act on an Open / Export As dialog once it has closed.
    fn poll_dialog(&mut self, ctx: &egui::Context) {
        let Some(result) = self.dialog.as_ref().and_then(|(_, d)| d.poll()) else {
            if self.dialog.is_some() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            return;
        };
        let Some((kind, _)) = self.dialog.take() else {
            return;
        };
        let Some(path) = result else { return }; // cancelled
        match kind {
            MenuDialog::Open => {
                let from_browser = match &mut self.screen {
                    Screen::Editor(ed) => {
                        ed.stop();
                        ed.from_browser
                    }
                    _ => self.browser.is_some(),
                };
                self.screen = self.open_editor(path, ctx, from_browser);
            }
            MenuDialog::ExportAs => {
                if let Screen::Editor(ed) = &mut self.screen {
                    ed.export(&self.settings, Some(with_mp4_extension(path)));
                }
            }
        }
    }

    fn back_to_browser(&mut self, ctx: &egui::Context) {
        let b = self
            .browser
            .get_or_insert_with(|| browser::Browser::new(&self.settings));
        b.refresh();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("Clipman".into()));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(1180.0, 780.0)));
        self.screen = Screen::Browser;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // settings dialog (modal over whatever is showing)
        if let Some(ui) = &mut self.settings_ui
            && let Some(outcome) = ui.show(ctx)
        {
            self.settings_ui = None;
            if let Outcome::Saved(s) = outcome {
                self.settings = s;
                crate::update::start(self.settings.check_updates); // if just turned on
                if let Screen::Editor(ed) = &mut self.screen {
                    ed.apply_settings(&self.settings);
                }
                if let Some(b) = &mut self.browser {
                    b.set_folder(self.settings.clips_folder());
                }
            }
            if matches!(self.screen, Screen::SettingsOnly) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        self.poll_dialog(ctx);
        // menu bar (and its shortcuts) over the browser and the editor
        if matches!(self.screen, Screen::Browser | Screen::Editor(_) | Screen::Error(_)) {
            let shortcut = self.menu_shortcuts(ctx);
            if let Some(cmd) = self.menu_bar(ctx).or(shortcut) {
                self.run_menu(cmd, ctx);
            }
        }

        match &mut self.screen {
            Screen::Browser => {
                let action = self
                    .browser
                    .as_mut()
                    .map(|b| b.ui(ctx))
                    .unwrap_or(browser::Action::None);
                match action {
                    browser::Action::Open(p) => self.screen = self.open_editor(p, ctx, true),
                    browser::Action::None => {}
                }
            }
            Screen::Editor(_) => {
                let settings = self.settings.clone();
                let _ = self.mixer(); // open the audio device once
                let mixer = self.audio.as_ref().map(|s| s.mixer());
                let Screen::Editor(ed) = &mut self.screen else {
                    unreachable!()
                };
                match ed.ui(ctx, &settings, mixer) {
                    editor::Action::None => {}
                    editor::Action::Back => {
                        ed.stop();
                        self.back_to_browser(ctx);
                    }
                    editor::Action::Exported => {
                        ed.stop();
                        if ed.from_browser {
                            self.back_to_browser(ctx);
                        } else {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                }
            }
            Screen::Render(rw) => {
                if rw.ui(ctx) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Screen::SettingsOnly => {
                egui::CentralPanel::default().show(ctx, |_| {});
            }
            Screen::Error(msg) => {
                let mut close = false;
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, msg.as_str());
                    close = ui.button("Close").clicked();
                });
                if close {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // don't leave ffmpeg running behind a closed window
        match &mut self.screen {
            Screen::Editor(ed) => ed.stop(),
            Screen::Render(rw) => rw.cancel.store(true, Ordering::Relaxed),
            _ => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------------------
// Progress window for renders started outside the editor
// ---------------------------------------------------------------------------

enum Msg {
    Progress(usize, f32, String),
    Finished(usize, Result<(PathBuf, u64), String>),
}

struct RenderWindow {
    names: Vec<String>,
    results: Vec<Option<Result<(PathBuf, u64), String>>>,
    current: usize,
    frac: f32,
    label: String,
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

impl RenderWindow {
    fn start(jobs: Vec<Job>) -> Self {
        let names = jobs
            .iter()
            .map(|j| {
                j.src
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let n = jobs.len();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            for (i, job) in jobs.iter().enumerate() {
                let t = tx.clone();
                let mut report = move |f: f32, l: &str| {
                    let _ = t.send(Msg::Progress(i, f, l.to_string()));
                };
                let r = render::render(job, &mut report, &c).map_err(|e| format!("{e:#}"));
                let cancelled = c.load(Ordering::Relaxed);
                let _ = tx.send(Msg::Finished(i, r));
                if cancelled {
                    break;
                }
            }
        });
        Self {
            names,
            results: vec![None; n],
            current: 0,
            frac: 0.0,
            label: "Starting…".into(),
            rx,
            cancel,
        }
    }

    /// Returns true when the window should close (everything succeeded).
    fn ui(&mut self, ctx: &egui::Context) -> bool {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Progress(i, f, l) => (self.current, self.frac, self.label) = (i, f, l),
                Msg::Finished(i, r) => self.results[i] = Some(r),
            }
        }
        let cancelled = self.cancel.load(Ordering::Relaxed);
        let done = self.results.iter().all(Option::is_some)
            || (cancelled && self.results.iter().any(Option::is_some));
        let all_ok = self.results.iter().all(|r| matches!(r, Some(Ok(_))));
        if done && all_ok {
            return true;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        let mut close = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            let n = self.names.len();
            if !done {
                let prefix = if n > 1 {
                    format!("[{}/{n}] ", self.current + 1)
                } else {
                    String::new()
                };
                ui.label(format!("{prefix}{}", self.names[self.current]));
                ui.add(egui::ProgressBar::new(self.frac).text(format!(
                    "{}  {:.0}%",
                    self.label,
                    self.frac * 100.0
                )));
                if ui
                    .add_enabled(!cancelled, egui::Button::new("Cancel"))
                    .clicked()
                {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (name, r) in self.names.iter().zip(&self.results) {
                        match r {
                            Some(Ok((p, size))) => {
                                ui.label(format!(
                                    "✔ {} ({:.2} MB)",
                                    p.file_name().unwrap_or_default().to_string_lossy(),
                                    *size as f64 / 1e6
                                ));
                            }
                            Some(Err(e)) => {
                                ui.colored_label(
                                    ui.visuals().error_fg_color,
                                    format!("✖ {name}: {e}"),
                                );
                            }
                            None => {
                                ui.weak(format!("– {name}: skipped"));
                            }
                        }
                    }
                });
                close = ui.button("Close").clicked();
            }
        });
        close
    }
}

/// `path` with `.mp4` added unless it already ends in it (added, not swapped in, so
/// "clip.final" becomes "clip.final.mp4").
fn with_mp4_extension(mut path: PathBuf) -> PathBuf {
    let is_mp4 = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    if !is_mp4 {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".mp4");
        path.set_file_name(name);
    }
    path
}

/// Show a folder in the file manager.
fn open_folder(dir: &std::path::Path) {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(windows, target_os = "macos")))]
    let program = "xdg-open";
    let _ = crate::media::quiet_command(program).arg(dir).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp4_extension() {
        let f = |p: &str| with_mp4_extension(PathBuf::from(p));
        assert_eq!(f("/x/clip"), PathBuf::from("/x/clip.mp4"));
        assert_eq!(f("/x/clip.final"), PathBuf::from("/x/clip.final.mp4"));
        assert_eq!(f("/x/clip.MP4"), PathBuf::from("/x/clip.MP4"));
        assert_eq!(f("/x/my clip.mp4"), PathBuf::from("/x/my clip.mp4"));
    }
}
