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
            .with_min_inner_size([480.0, 200.0]),
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
        };
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

        match &mut self.screen {
            Screen::Browser => {
                let action = self
                    .browser
                    .as_mut()
                    .map(|b| b.ui(ctx))
                    .unwrap_or(browser::Action::None);
                match action {
                    browser::Action::Open(p) => self.screen = self.open_editor(p, ctx, true),
                    browser::Action::OpenSettings => {
                        self.settings_ui = Some(SettingsUi::new(&self.settings, &[]))
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
                match ed.ui(ctx, &settings, mixer) {
                    editor::Action::None => {}
                    editor::Action::OpenSettings => {
                        self.settings_ui = Some(SettingsUi::new(&self.settings, ed.track_names()));
                    }
                    editor::Action::Back => {
                        ed.stop();
                        self.back_to_browser(ctx);
                    }
                    editor::Action::Saved => {
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
