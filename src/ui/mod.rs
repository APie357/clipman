// SPDX-License-Identifier: Apache-2.0
//! The native window: clip browser, clip editor, and a progress window for renders
//! started from the command line / right-click menu.

mod browser;
mod editor;
mod plugins;
mod settings;
mod timeline;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, Vec2};

use crate::plugin::{self, Host};
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
    /// A plugin's menu item: (plugin, item).
    Plugin(usize, usize),
    ReloadPlugins,
    OpenPluginsFolder,
    ManagePlugins,
}

/// Height of a menu entry.
const MENU_ITEM_HEIGHT: f32 = 20.0;

/// How long a status message (from a plugin) stays in the menu bar.
const NOTICE_TIME: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// `--plugin-action`: run a plugin's clip action, show its windows, then close.
    PluginAction {
        plugin: String,
        action: String,
        files: Vec<PathBuf>,
    },
}

enum Screen {
    Browser,
    Editor(Box<editor::Editor>),
    Render(RenderWindow),
    /// Only the settings dialog (`clipman --settings`).
    SettingsOnly,
    /// A plugin action from the file manager: its windows, if it opens any.
    PluginAction,
    Error(String),
}

pub struct App {
    settings: Settings,
    screen: Screen,
    browser: Option<browser::Browser>,
    settings_ui: Option<SettingsUi>,
    /// The Plugins dialog (over the Settings dialog when opened from it).
    plugins_ui: Option<plugins::PluginsUi>,
    audio: Option<rodio::MixerDeviceSink>,
    audio_tried: bool,
    /// An Open / Export As dialog that's open.
    dialog: Option<(MenuDialog, PendingDialog)>,
    plugins: Host,
    /// The latest plugin message and when it came.
    notice: Option<(String, std::time::Instant)>,
    /// Close once plugins are done (no processes running, no windows open).
    close_when_idle: bool,
}

pub fn initial_size(start: &Start) -> Vec2 {
    match start {
        Start::Browser => Vec2::new(1180.0, 780.0),
        Start::Editor(p) => crate::media::probe(p)
            .map(|i| editor::Editor::initial_window_size(&i))
            .unwrap_or(Vec2::new(1296.0, 870.0)),
        Start::Render(_) => Vec2::new(560.0, 220.0),
        Start::Settings => Vec2::new(920.0, 760.0),
        Start::PluginAction { .. } => Vec2::new(640.0, 420.0),
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

pub fn run(start: Start, mut plugins: Host) -> eframe::Result<()> {
    plugins.set_gui(true);
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
        // With vsync, presenting waits for the compositor's next frame callback, and
        // Wayland compositors send none to a window that isn't visible (another
        // workspace): the UI thread then blocks, stops answering the compositor's pings,
        // and Hyprland reports clipman as not responding. Frame pacing comes from
        // request_repaint_after instead.
        vsync: false,
        ..Default::default()
    };
    eframe::run_native(
        "clipman",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(start, &cc.egui_ctx, plugins)))),
    )
}

impl App {
    fn new(start: Start, ctx: &egui::Context, plugins: Host) -> Self {
        apply_style(ctx);
        let settings = Settings::load();
        let mut app = Self {
            settings,
            screen: Screen::Browser,
            browser: None,
            settings_ui: None,
            plugins_ui: None,
            audio: None,
            audio_tried: false,
            dialog: None,
            plugins,
            notice: None,
            close_when_idle: false,
        };
        if !matches!(start, Start::Render(_)) {
            crate::update::start(app.settings.check_updates);
        }
        app.screen = match start {
            Start::Browser => {
                app.browser = Some(browser::Browser::new(&app.settings));
                Screen::Browser
            }
            Start::PluginAction {
                plugin,
                action,
                files,
            } => {
                let pctx = plugin::Ctx {
                    screen: plugin::Screen::Other,
                    clip: None,
                };
                match app.plugins.run_clip_action(&plugin, &action, &files, &pctx) {
                    Ok(()) => {
                        app.close_when_idle = true;
                        Screen::PluginAction
                    }
                    Err(e) => Screen::Error(format!("Plugin action {plugin}:{action}: {e}")),
                }
            }
            Start::Editor(path) => app.open_editor(path, ctx, false),
            Start::Render(jobs) => Screen::Render(RenderWindow::start(jobs)),
            Start::Settings => {
                app.settings_ui = Some(app.settings_dialog(&[]));
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

    fn settings_dialog(&self, tracks: &[Option<String>]) -> SettingsUi {
        SettingsUi::new(
            &self.settings,
            tracks,
            self.plugins.infos().len(),
            self.plugins.clip_actions(),
        )
    }

    /// What plugin callbacks see: the screen, and the clip if the editor is open.
    fn plugin_ctx(&self) -> plugin::Ctx {
        match &self.screen {
            Screen::Editor(ed) => plugin::Ctx {
                screen: plugin::Screen::Editor,
                clip: Some(ed.clip_state()),
            },
            Screen::Browser => plugin::Ctx {
                screen: plugin::Screen::Browser,
                clip: None,
            },
            _ => plugin::Ctx {
                screen: plugin::Screen::Other,
                clip: None,
            },
        }
    }

    fn show_notices(&mut self) {
        if let Some(n) = self.plugins.take_notices().pop() {
            self.notice = Some((n, std::time::Instant::now()));
        }
    }

    fn open_editor(&mut self, path: PathBuf, ctx: &egui::Context, from_browser: bool) -> Screen {
        match editor::Editor::new(path.clone(), &self.settings, ctx, from_browser) {
            Ok(ed) => {
                self.plugins.clip_opened(&plugin::Ctx {
                    screen: plugin::Screen::Editor,
                    clip: Some(ed.clip_state()),
                });
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
                // square corners on File / Help
                let w = &mut ui.visuals_mut().widgets;
                for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                    v.corner_radius = egui::CornerRadius::ZERO;
                }
                let item = menu_item;
                ui.menu_button("File", |ui| {
                    menu_style(ui);
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
                let screen = if editor.is_some() {
                    plugin::Screen::Editor
                } else {
                    plugin::Screen::Browser
                };
                // plugins' own dropdowns
                for m in self.plugins.menus(screen) {
                    ui.menu_button(m.title.as_str(), |ui| {
                        menu_style(ui);
                        plugin_entries(ui, &m.entries, !busy, &mut cmd);
                    });
                }
                if self.plugins.is_active() {
                    ui.menu_button("Plugins", |ui| {
                        menu_style(ui);
                        if item(ui, true, "Manage plugins\u{2026}", None) {
                            cmd = Some(MenuCmd::ManagePlugins);
                        }
                        ui.separator();
                        if item(ui, !busy, "Reload plugins", None) {
                            cmd = Some(MenuCmd::ReloadPlugins);
                        }
                        if item(ui, true, "Open plugins folder", None) {
                            cmd = Some(MenuCmd::OpenPluginsFolder);
                        }
                    });
                }
                ui.menu_button("Help", |ui| {
                    menu_style(ui);
                    ui.label(format!("Clipman {}", crate::update::CURRENT));
                    if let crate::update::Status::Available(r) = crate::update::status() {
                        ui.hyperlink_to(format!("Clipman {} is available", r.version), &r.url);
                    }
                    ui.hyperlink_to(
                        "Project page",
                        format!("https://github.com/{}", crate::update::REPO),
                    );
                });
                if let Some((text, at)) = &self.notice
                    && at.elapsed() < NOTICE_TIME
                {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(text.as_str()).truncate());
                    });
                    ui.ctx().request_repaint_after(NOTICE_TIME - at.elapsed());
                }
            });
        });
        cmd
    }

    /// Menu shortcuts, checked every frame (also when the menu is closed).
    fn menu_shortcuts(&self, ctx: &egui::Context) -> Option<MenuCmd> {
        if self.settings_ui.is_some() || self.plugins_ui.is_some() || self.dialog.is_some() {
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

    /// Plugin menu shortcuts (checked after the built-in ones, which always win). Every
    /// press counts: two quick shortcuts can arrive in the same frame.
    fn plugin_shortcuts(&self, ctx: &egui::Context) -> Vec<MenuCmd> {
        if self.settings_ui.is_some()
            || self.plugins_ui.is_some()
            || self.dialog.is_some()
            || matches!(&self.screen, Screen::Editor(ed) if ed.is_exporting())
        {
            return Vec::new();
        }
        let screen = match self.screen {
            Screen::Editor(_) => plugin::Screen::Editor,
            _ => plugin::Screen::Browser,
        };
        let mut items: Vec<_> = self
            .plugins
            .shortcut_items(screen)
            .into_iter()
            .filter_map(|i| Some((i.shortcut?, i.plugin, i.item)))
            .collect();
        // more modifiers first: matching ignores Shift unless asked for
        items.sort_by_key(|(sc, ..)| std::cmp::Reverse(modifier_count(sc.modifiers)));
        ctx.input_mut(|inp| {
            let mut out = Vec::new();
            for (sc, p, i) in items {
                let n = inp.count_and_consume_key(sc.modifiers, sc.logical_key);
                out.extend(std::iter::repeat_n(MenuCmd::Plugin(p, i), n));
            }
            out
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
                    ed.export(&self.settings, None, &mut self.plugins);
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
                open_path(&dir);
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
                    self.settings_ui = Some(self.settings_dialog(&tracks));
                }
            }
            MenuCmd::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            MenuCmd::Plugin(p, i) => {
                let pctx = self.plugin_ctx();
                self.plugins.run_menu(p, i, &pctx);
            }
            MenuCmd::ReloadPlugins => {
                self.plugins.reload(&self.settings);
                let n = self.plugins.infos().iter().filter(|i| i.enabled).count();
                let failed = self.plugins.infos().iter().filter(|i| i.error.is_some()).count();
                let mut text = format!("Loaded {n} plugin{}", if n == 1 { "" } else { "s" });
                if failed > 0 {
                    text += &format!(", {failed} with errors (see Plugins, Manage plugins)");
                }
                self.plugins.take_notices();
                self.notice = Some((text, std::time::Instant::now()));
            }
            MenuCmd::OpenPluginsFolder => {
                let dir = self.plugins.dir().to_path_buf();
                let _ = std::fs::create_dir_all(&dir);
                open_path(&dir);
            }
            MenuCmd::ManagePlugins => self.open_plugins_dialog(),
        }
    }

    fn settings_closed(&mut self, outcome: Outcome, ctx: &egui::Context) {
        self.settings_ui = None;
        self.plugins_ui = None;
        if let Outcome::Saved(s) = outcome {
            self.settings = *s;
            self.plugins.apply_settings(&self.settings);
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

    fn open_plugins_dialog(&mut self) {
        if self.plugins_ui.is_none() {
            if let Screen::Editor(ed) = &mut self.screen {
                ed.pause();
            }
            self.plugins_ui = Some(plugins::PluginsUi::new(
                self.plugins.infos(),
                &self.settings.plugins,
                self.settings.clips_folder(),
                self.plugins.dir().to_path_buf(),
            ));
        }
    }

    /// Save the plugin settings from the Plugins dialog and load / unload what changed.
    fn apply_plugin_settings(
        &mut self,
        map: indexmap::IndexMap<String, crate::settings::PluginCfg>,
    ) -> Result<(), String> {
        let mut s = self.settings.clone();
        s.plugins = map;
        s.save().map_err(|e| format!("Couldn't save settings: {e}"))?;
        self.settings = s;
        self.plugins.apply_settings(&self.settings);
        self.plugins.take_notices();
        if let Some(sui) = &mut self.settings_ui {
            sui.set_plugins(
                &self.settings.plugins,
                self.plugins.infos().len(),
                self.plugins.clip_actions(),
            );
        }
        Ok(())
    }

    /// The Plugins dialog, if open.
    fn plugins_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.plugins_ui.as_mut().and_then(|p| p.show(ctx)) else {
            return;
        };
        match action {
            plugins::Action::Cancel => self.plugins_ui = None,
            plugins::Action::Ok(map) => match self.apply_plugin_settings(map) {
                Ok(()) => self.plugins_ui = None,
                Err(e) => self.plugins_ui.as_mut().unwrap().message = Some(e),
            },
            plugins::Action::Apply(map) => {
                let r = self.apply_plugin_settings(map);
                let infos = self.plugins.infos();
                let pui = self.plugins_ui.as_mut().unwrap();
                pui.message = r.err();
                pui.applied(infos, &self.settings.plugins);
            }
            plugins::Action::Reload => {
                self.plugins.reload(&self.settings);
                self.plugins.take_notices();
                if let Some(sui) = &mut self.settings_ui {
                    sui.set_plugins(
                &self.settings.plugins,
                self.plugins.infos().len(),
                self.plugins.clip_actions(),
            );
                }
                let infos = self.plugins.infos();
                let pui = self.plugins_ui.as_mut().unwrap();
                pui.message = Some(format!(
                    "Reloaded {} plugin{}",
                    infos.len(),
                    if infos.len() == 1 { "" } else { "s" }
                ));
                pui.applied(infos, &self.settings.plugins);
            }
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
                    ed.export(&self.settings, Some(with_mp4_extension(path)), &mut self.plugins);
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
    /// What shows where no panel paints (the editor's margins and timeline background).
    /// eframe's default is slightly transparent, and Wayland compositors show the
    /// desktop through it.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::from_rgb(12, 12, 12).to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // settings dialog (modal over whatever is showing)
        let covered = self.plugins_ui.is_some();
        if let Some(ui) = &mut self.settings_ui
            && let Some(outcome) = ui.show(ctx, covered)
        {
            if matches!(outcome, Outcome::ManagePlugins) {
                self.open_plugins_dialog(); // drawn on top, below
            } else {
                self.settings_closed(outcome, ctx);
            }
        }
        // plugins dialog: after the settings dialog, so it's on top of it
        self.plugins_dialog(ctx);

        self.poll_dialog(ctx);
        // menu bar (and its shortcuts) over the browser and the editor
        if matches!(self.screen, Screen::Browser | Screen::Editor(_) | Screen::Error(_)) {
            let mut cmds: Vec<MenuCmd> = self.menu_shortcuts(ctx).into_iter().collect();
            cmds.extend(self.plugin_shortcuts(ctx));
            if let Some(cmd) = self.menu_bar(ctx) {
                cmds = vec![cmd]; // a click in the menu, keys went to it
            }
            for cmd in cmds {
                self.run_menu(cmd, ctx);
            }
        }

        match &mut self.screen {
            Screen::Browser => {
                let action = self
                    .browser
                    .as_mut()
                    .map(|b| b.ui(ctx, &mut self.plugins))
                    .unwrap_or(browser::Action::None);
                match action {
                    browser::Action::Open(p) => self.screen = self.open_editor(p, ctx, true),
                    browser::Action::Plugin(a, path) => {
                        let pctx = self.plugin_ctx();
                        if let Err(e) =
                            self.plugins
                                .run_clip_action(&a.plugin_id, &a.id, &[path], &pctx)
                        {
                            self.notice = Some((e, std::time::Instant::now()));
                        }
                    }
                    browser::Action::CardSize(w) => {
                        self.settings.card_size = w;
                        if let Err(e) = self.settings.save() {
                            self.notice = Some((
                                format!("Couldn't save settings: {e}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
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
                match ed.ui(ctx, &settings, mixer, &mut self.plugins) {
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
                            self.close_when_idle = true;
                        }
                    }
                }
            }
            Screen::Render(rw) => {
                if rw.ui(ctx, &mut self.plugins) {
                    self.close_when_idle = true;
                }
            }
            Screen::SettingsOnly => {
                egui::CentralPanel::default().show(ctx, |_| {});
            }
            Screen::PluginAction => {
                let notice = self.notice.as_ref().map(|(n, _)| n.clone());
                egui::CentralPanel::default().show(ctx, |ui| {
                    if self.plugins.busy() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Running plugin\u{2026}");
                        });
                    }
                    if let Some(n) = notice {
                        ui.label(n);
                    }
                });
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

        // plugins: finished processes, their windows, their messages
        if self.plugins.poll() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let pctx = self.plugin_ctx();
        self.plugins.draw_windows(ctx, &pctx);
        if self.plugins.has_clip_cmds() {
            ctx.request_repaint(); // the editor applies them next frame
        }
        self.show_notices();
        if self.close_when_idle && !self.plugins.busy() && !self.plugins.has_open_windows() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
    jobs: Vec<Job>,
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
        let kept = jobs.clone();
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
            jobs: kept,
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
    fn ui(&mut self, ctx: &egui::Context, plugins: &mut Host) -> bool {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Progress(i, f, l) => (self.current, self.frac, self.label) = (i, f, l),
                Msg::Finished(i, r) => {
                    plugins.export_done(&self.jobs[i], &r);
                    self.results[i] = Some(r);
                }
            }
        }
        let cancelled = self.cancel.load(Ordering::Relaxed);
        let done = self.results.iter().all(Option::is_some)
            || (cancelled && self.results.iter().any(Option::is_some));
        let all_ok = self.results.iter().all(|r| matches!(r, Some(Ok(_))));
        if done && all_ok {
            if plugins.busy() || plugins.has_open_windows() {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Done. Waiting for plugins\u{2026}");
                    });
                });
            }
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

/// Show a folder in the file manager (or open a file / URL with its default app).
pub fn open_path(dir: &std::path::Path) {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(windows, target_os = "macos")))]
    let program = "xdg-open";
    let _ = crate::media::quiet_command(program).arg(dir).spawn();
}

/// A menu entry with its shortcut shown on the right. True when clicked.
fn menu_item(
    ui: &mut egui::Ui,
    enabled: bool,
    text: &str,
    sc: Option<egui::KeyboardShortcut>,
) -> bool {
    let mut b = egui::Button::new(text);
    if let Some(sc) = sc {
        // "Ctrl+Comma" reads better as "Ctrl+,"
        b = b.shortcut_text(ui.ctx().format_shortcut(&sc).replace("Comma", ","));
    }
    ui.add_enabled(enabled, b).clicked()
}

/// The entries of a plugin's own dropdown (submenus recursively).
fn plugin_entries(
    ui: &mut egui::Ui,
    entries: &[plugin::MenuEntry],
    enabled: bool,
    cmd: &mut Option<MenuCmd>,
) {
    for e in entries {
        match e {
            plugin::MenuEntry::Item(it) => {
                if menu_item(ui, enabled, &it.label, it.shortcut) {
                    *cmd = Some(MenuCmd::Plugin(it.plugin, it.item));
                }
            }
            plugin::MenuEntry::Separator => {
                ui.separator();
            }
            plugin::MenuEntry::Submenu(label, sub) => {
                ui.menu_button(label.as_str(), |ui| {
                    menu_style(ui);
                    plugin_entries(ui, sub, enabled, cmd);
                });
            }
        }
    }
}

/// Style for the inside of a menu: grows to fit its items instead of wrapping long labels,
/// and entries are shorter than the app's other controls (CONTROL_HEIGHT).
fn menu_style(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.wrap_mode = Some(egui::TextWrapMode::Extend);
    style.spacing.interact_size.y = MENU_ITEM_HEIGHT;
    style.spacing.button_padding = Vec2::new(8.0, 2.0);
}

fn modifier_count(m: egui::Modifiers) -> u32 {
    [m.command || m.ctrl, m.shift, m.alt].into_iter().map(u32::from).sum()
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
