// SPDX-License-Identifier: Apache-2.0
//! The clip editor: preview, trim handles, audio track selection with noise
//! suppression, preset choice, and rendering with progress shown in the window.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, Color32, Key, Vec2};
use egui::containers::menu::{MenuButton, MenuConfig};

use super::timeline::{Drag, Timeline};
use crate::media::{self, ClipInfo, fmt_time};
use crate::player::Player;
use crate::render::{self, AudioPlan, Job};
use crate::settings::{Denoise, Settings};

const AUDIO_BTN_MAX_CHARS: usize = 44; // long track names would stretch the window
pub const PREVIEW_MAX_WIDTH: u32 = 1280;

pub enum Action {
    None,
    /// Go back to the clip browser (if we came from it).
    Back,
    /// Export finished: close (standalone) or go back to the browser.
    Exported,
}

enum RenderMsg {
    Progress(f32, String),
    Done(Result<(PathBuf, u64), String>),
}

struct RenderTask {
    rx: Receiver<RenderMsg>,
    cancel: Arc<AtomicBool>,
    frac: f32,
    label: String,
}

struct TrackUi {
    label: String,
    on: bool,
    /// None = noise suppression not offered for this track.
    ns: Option<bool>,
}

pub struct Editor {
    pub path: PathBuf,
    pub info: ClipInfo,
    /// Every frame's timestamp and the keyframes (frame stepping, lossless cuts).
    frames: media::FrameTimes,
    player: Player,
    texture: Option<egui::TextureHandle>,
    timeline: Timeline,
    tracks: Vec<TrackUi>,
    preset_id: Option<String>,
    render: Option<RenderTask>,
    status: String,
    model_fetch: Option<std::thread::JoinHandle<()>>,
    /// Remaining attempts at sizing the window so the video area has the clip's aspect.
    fit_tries: u8,
    pub from_browser: bool,
}

impl Editor {
    pub fn new(
        path: PathBuf,
        settings: &Settings,
        ctx: &egui::Context,
        from_browser: bool,
    ) -> anyhow::Result<Self> {
        let info = media::probe(&path)?;
        let frames = media::frame_times(&path);
        let width = info.width.clamp(320, PREVIEW_MAX_WIDTH);
        let player = Player::new(path.clone(), info.duration, info.fps, width, ctx.clone());
        let mut ed = Self {
            timeline: Timeline::new(info.duration),
            path,
            info,
            frames,
            player,
            texture: None,
            tracks: Vec::new(),
            preset_id: None,
            render: None,
            status: String::new(),
            model_fetch: None,
            fit_tries: 3,
            from_browser,
        };
        ed.apply_settings(settings);
        Ok(ed)
    }

    /// Window size that shows the video at up to 1280 px wide with the clip's aspect.
    pub fn initial_window_size(info: &ClipInfo) -> Vec2 {
        let w = 1280.0;
        Vec2::new(w + 16.0, w / info.aspect() + 150.0)
    }

    /// (Re)build the track list and preset choice after settings changed.
    pub fn apply_settings(&mut self, settings: &Settings) {
        self.player.set_gpu_playback(settings.gpu_playback);
        self.tracks = self
            .info
            .audio_names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let t = settings.track(n.as_deref(), i);
                TrackUi {
                    label: t.label,
                    on: t.default,
                    ns: (t.denoise != Denoise::None).then_some(t.denoise == Denoise::On),
                }
            })
            .collect();
        if self
            .preset_id
            .as_deref()
            .and_then(|id| settings.preset(id))
            .is_none()
        {
            self.preset_id = settings.presets.first().map(|p| p.id.clone());
        }
        if !self.denoise().is_empty() {
            self.prefetch_model();
        }
    }

    fn selected(&self) -> Vec<usize> {
        (0..self.tracks.len())
            .filter(|&i| self.tracks[i].on)
            .collect()
    }

    fn denoise(&self) -> Vec<usize> {
        (0..self.tracks.len())
            .filter(|&i| self.tracks[i].on && self.tracks[i].ns == Some(true))
            .collect()
    }

    fn prefetch_model(&mut self) {
        if self.model_fetch.is_none() && !render::model_ready() {
            self.model_fetch = Some(std::thread::spawn(|| {
                let _ = render::ensure_model();
            }));
        }
    }

    /// The audio the preview plays: the same mix as the render. Noise suppression is
    /// only included once the model is downloaded.
    fn preview_plan(&self, settings: &Settings) -> AudioPlan {
        let dn = if render::model_ready() {
            self.denoise()
        } else {
            vec![]
        };
        render::audio_plan(&self.selected(), &dn, settings.denoise_strength)
    }

    pub fn track_names(&self) -> &[Option<String>] {
        &self.info.audio_names
    }

    pub fn stop(&mut self) {
        self.player.pause();
        if let Some(r) = &self.render {
            r.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn toggle(&mut self, settings: &Settings, mixer: Option<&rodio::mixer::Mixer>) {
        if self.player.is_playing() {
            self.player.pause();
            return;
        }
        let mut t = self.player.position();
        if t < self.timeline.start || t >= self.timeline.end - 0.05 {
            t = self.timeline.start;
        }
        let plan = self.preview_plan(settings);
        self.player.play(t, mixer.map(|m| (m, &plan)));
    }

    fn seek(&mut self, t: f64, settings: &Settings, mixer: Option<&rodio::mixer::Mixer>) {
        let plan = self.preview_plan(settings);
        self.player.seek(t, mixer.map(|m| (m, &plan)));
    }

    fn preset<'a>(&self, settings: &'a Settings) -> Option<&'a crate::settings::Preset> {
        self.preset_id.as_deref().and_then(|id| settings.preset(id))
    }

    /// Whether Export / Export As can start (a preset is chosen and nothing's rendering).
    pub fn can_export(&self, settings: &Settings) -> bool {
        self.render.is_none() && self.preset(settings).is_some()
    }

    pub fn is_exporting(&self) -> bool {
        self.render.is_some()
    }

    /// Where Export would save (the export folder, named after the clip and preset).
    pub fn default_export_path(&self, settings: &Settings) -> Option<PathBuf> {
        let p = self.preset(settings)?;
        Some(settings.output_path(&self.path, &p.suffix))
    }

    pub fn pause(&mut self) {
        self.player.pause();
    }

    /// Export with the chosen preset to `dst`, or to the export folder when None.
    pub fn export(&mut self, settings: &Settings, dst: Option<PathBuf>) {
        if !self.can_export(settings) {
            return;
        }
        let Some(preset) = self.preset(settings).cloned() else {
            return;
        };
        let abs = |p: &std::path::Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if let Some(d) = &dst
            && abs(d) == abs(&self.path)
        {
            self.status = "Can\u{2019}t export over the original clip: pick another name.".into();
            return;
        }
        self.player.pause();
        let job = Job {
            src: self.path.clone(),
            dst: dst.unwrap_or_else(|| settings.output_path(&self.path, &preset.suffix)),
            preset,
            start: Some(self.timeline.start),
            end: Some(self.timeline.end),
            tracks: self.selected(),
            denoise: self.denoise(),
            strength: settings.denoise_strength,
            encoder: settings.encoder,
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let mut report = move |f: f32, l: &str| {
                let _ = tx2.send(RenderMsg::Progress(f, l.to_string()));
            };
            let res = render::render(&job, &mut report, &c).map_err(|e| {
                if e.is::<render::Cancelled>() {
                    "Cancelled".to_string()
                } else {
                    format!("{e:#}")
                }
            });
            let _ = tx.send(RenderMsg::Done(res));
        });
        self.status.clear();
        self.render = Some(RenderTask {
            rx,
            cancel,
            frac: 0.0,
            label: "Starting…".into(),
        });
    }

    fn range_text(&self, settings: &Settings) -> String {
        let (s, e) = (self.timeline.start, self.timeline.end);
        let span = format!("{} – {}", fmt_time(s), fmt_time(e));
        let Some(p) = self.preset_id.as_deref().and_then(|id| settings.preset(id)) else {
            return "No presets: add one in Settings".into();
        };
        if p.is_original() {
            let k = media::keyframe_before(&self.frames.keys, s);
            let note = if s - k > 0.02 {
                format!("starts at {} (keyframe)", fmt_time(k))
            } else {
                "exact start".into()
            };
            return format!("{span}   ({:.1}s, source, {note})", e - k);
        }
        let len = (e - s).max(0.001);
        match p.limit_mb() {
            None => format!("{span}   ({len:.1}s, no size limit)"),
            Some(mb) => {
                let kbps = (mb * 0.97 * 8000.0 / len - 96.0).min(16000.0);
                format!(
                    "{span}   ({len:.1}s, max {}, ~{:.1} Mbps)",
                    p.size_text(),
                    kbps / 1000.0
                )
            }
        }
    }

    fn audio_button_text(&self) -> String {
        if self.tracks.is_empty() {
            return "Audio: none in clip".into();
        }
        let names: Vec<String> = self
            .tracks
            .iter()
            .filter(|t| t.on)
            .map(|t| {
                if t.ns == Some(true) {
                    format!("{} (RNNoise)", t.label)
                } else {
                    t.label.clone()
                }
            })
            .collect();
        let full = format!(
            "Audio: {}",
            if names.is_empty() {
                "muted".into()
            } else {
                names.join(" + ")
            }
        );
        if full.chars().count() > AUDIO_BTN_MAX_CHARS {
            format!(
                "{}…",
                full.chars()
                    .take(AUDIO_BTN_MAX_CHARS - 1)
                    .collect::<String>()
            )
        } else {
            full
        }
    }

    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        settings: &Settings,
        mixer: Option<&rodio::mixer::Mixer>,
    ) -> Action {
        let mut action = Action::None;

        // render progress
        if let Some(task) = &mut self.render {
            let mut done = None;
            while let Ok(msg) = task.rx.try_recv() {
                match msg {
                    RenderMsg::Progress(f, l) => (task.frac, task.label) = (f, l),
                    RenderMsg::Done(r) => done = Some(r),
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            if let Some(r) = done {
                self.render = None;
                match r {
                    Ok(_) => return Action::Exported,
                    Err(e) => {
                        self.status = if e == "Cancelled" {
                            "Cancelled.".into()
                        } else {
                            format!("Export failed: {e}")
                        }
                    }
                }
            }
        }
        let busy = self.render.is_some();

        // new video frame
        if let Some(f) = self.player.take_frame() {
            let img = egui::ColorImage::from_rgb([f.width, f.height], &f.rgb);
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("frame", img, egui::TextureOptions::LINEAR))
                }
            }
        }
        if self.player.is_playing() {
            if self.player.position() >= self.timeline.end {
                self.player.pause();
                self.seek(self.timeline.end, settings, mixer);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        // keyboard
        if std::env::var_os("CLIPMAN_DEBUG").is_some() {
            ctx.input(|inp| {
                for e in &inp.events {
                    if let egui::Event::Key {
                        key,
                        pressed,
                        modifiers,
                        ..
                    } = e
                    {
                        eprintln!("key {key:?} pressed={pressed} {modifiers:?}");
                    }
                }
            });
        }
        if !busy && !ctx.wants_keyboard_input() {
            // consume the keys so a focused button doesn't also react to them (Space would
            // otherwise toggle twice), and use each event's own modifiers (a quick Ctrl+S can
            // release Ctrl before the frame is drawn); counts include key-repeat
            use egui::Modifiers as M;
            let (space, i, o, step) = ctx.input_mut(|inp| {
                let mut n = |m, k| inp.count_and_consume_key(m, k) as i32;
                let space = n(M::NONE, Key::Space) > 0;
                let (i, o) = (n(M::NONE, Key::I) > 0, n(M::NONE, Key::O) > 0);
                // Shift first: egui's matching ignores Shift unless asked for, so the plain
                // check would also eat Shift+arrow
                let secs = n(M::SHIFT, Key::ArrowRight) - n(M::SHIFT, Key::ArrowLeft);
                let frames = n(M::NONE, Key::ArrowRight) - n(M::NONE, Key::ArrowLeft);
                (space, i, o, (frames, secs))
            });
            let pos = self.player.position();
            if space {
                self.toggle(settings, mixer);
            }
            if i && pos < self.timeline.end - super::timeline::MIN_LEN {
                self.timeline.start = pos;
            }
            if o && pos > self.timeline.start + super::timeline::MIN_LEN {
                self.timeline.end = pos;
            }
            // ←/→ step one frame (to the real neighbouring frame), Shift+←/→ one second
            if step.1 != 0 {
                let t = self.player.position() + step.1 as f64;
                self.seek(t.clamp(0.0, self.info.duration), settings, mixer);
            } else if step.0 != 0 && !self.frames.all.is_empty() {
                let all = &self.frames.all;
                let i = media::frame_index(all, self.player.position()) as i64 + step.0 as i64;
                let t = all[i.clamp(0, all.len() as i64 - 1) as usize];
                let plan = self.preview_plan(settings);
                self.player.step_to(t, mixer.map(|m| (m, &plan)));
            }
        }

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    if self.from_browser && ui.button("Back to clips").clicked() {
                        action = Action::Back;
                    }
                    if play_button(ui, self.player.is_playing()).clicked() {
                        self.toggle(settings, mixer);
                    }
                    ui.monospace(format!(
                        "{} / {}",
                        fmt_time(self.player.position()),
                        fmt_time(self.info.duration)
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.can_export(settings), egui::Button::new("Export"))
                            .on_hover_text(format!(
                                "Export to {} (Ctrl+E). To pick the place: File, Export As\u{2026}",
                                settings.export_folder().display()
                            ))
                            .clicked()
                        {
                            self.export(settings, None);
                        }
                        self.preset_combo(ui, settings);
                        self.audio_menu(ui, settings, mixer);
                    });
                });
            });
            ui.horizontal(|ui| {
                if let Some(task) = &self.render {
                    if ui.button("Cancel").clicked() {
                        task.cancel.store(true, Ordering::Relaxed);
                    }
                    ui.add(
                        egui::ProgressBar::new(task.frac)
                            .show_percentage()
                            .text(format!("{}  {:.0}%", task.label, task.frac * 100.0))
                            .desired_width(ui.available_width()),
                    );
                } else {
                    ui.label(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(self.range_text(settings));
                    });
                }
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(8.0))
            .show(ctx, |ui| {
                let avail = ui.available_size();
                let video_h_avail = (avail.y - Timeline::height() - 6.0).max(50.0);
                let aspect = self.info.aspect();
                self.fit_window(ctx, Vec2::new(avail.x, video_h_avail), aspect);
                let (area, _) =
                    ui.allocate_exact_size(Vec2::new(avail.x, video_h_avail), egui::Sense::hover());
                ui.painter().rect_filled(area, 0.0, Color32::BLACK);
                let w = area.width().min(area.height() * aspect);
                let img_rect =
                    egui::Rect::from_center_size(area.center(), Vec2::new(w, w / aspect));
                if let Some(tex) = &self.texture {
                    ui.painter().image(
                        tex.id(),
                        img_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                ui.add_space(6.0);
                ui.add_enabled_ui(!busy, |ui| {
                    let ev = self.timeline.ui(ui, self.player.position());
                    match ev.changed {
                        Some(Drag::Start) => {
                            self.player.pause();
                            self.seek(self.timeline.start, settings, mixer);
                        }
                        Some(Drag::End) => {
                            self.player.pause();
                            self.seek(self.timeline.end, settings, mixer);
                        }
                        Some(Drag::Seek) => {
                            if let Some(t) = ev.seek_to {
                                self.seek(t, settings, mixer);
                            }
                        }
                        None => {}
                    }
                });
            });
        action
    }

    /// Resize the window once so the video area has exactly the clip's aspect ratio
    /// (no black bars). Tiled/maximized windows simply ignore the request.
    fn fit_window(&mut self, ctx: &egui::Context, area: Vec2, aspect: f32) {
        if self.fit_tries == 0 {
            return;
        }
        self.fit_tries -= 1;
        let Some(inner) = ctx.input(|i| i.viewport().inner_rect) else {
            return;
        };
        let monitor = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or(Vec2::new(2560.0, 1440.0));
        let chrome = inner.size() - area;
        let want_w = (PREVIEW_MAX_WIDTH as f32)
            .min(monitor.x - chrome.x - 80.0)
            .min((monitor.y - chrome.y - 80.0) * aspect);
        let want = Vec2::new(want_w, want_w / aspect) + chrome;
        if (want - inner.size()).length() > 2.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
        }
    }

    fn preset_combo(&mut self, ui: &mut egui::Ui, settings: &Settings) {
        let current = self.preset_id.as_deref().and_then(|id| settings.preset(id));
        let text = current
            .map(|p| p.menu_text())
            .unwrap_or_else(|| "No presets".into());
        egui::ComboBox::from_id_salt("preset")
            .selected_text(text)
            .width(250.0)
            .show_ui(ui, |ui| {
                for p in &settings.presets {
                    ui.selectable_value(&mut self.preset_id, Some(p.id.clone()), p.menu_text());
                }
            });
    }

    fn audio_menu(
        &mut self,
        ui: &mut egui::Ui,
        settings: &Settings,
        mixer: Option<&rodio::mixer::Mixer>,
    ) {
        let mut changed = false;
        let mut want_model = false;
        let full_names: Vec<String> = self
            .tracks
            .iter()
            .filter(|t| t.on)
            .map(|t| t.label.clone())
            .collect();
        let resp = MenuButton::new(self.audio_button_text())
            .config(MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
            .ui(ui, |ui| {
                if self.tracks.is_empty() {
                    ui.weak("This clip has no audio");
                }
                for (i, t) in self.tracks.iter_mut().enumerate() {
                    ui.add_space(2.0);
                    changed |= ui
                        .checkbox(&mut t.on, format!("{}: {}", i + 1, t.label))
                        .changed();
                    if let Some(ns) = &mut t.ns {
                        ui.horizontal(|ui| {
                            ui.add_space(24.0);
                            ui.add_enabled_ui(t.on, |ui| {
                                let r = ui.checkbox(ns, "Noise suppression").on_hover_text(
                                    "RNNoise speech denoising (the model downloads on first use)",
                                );
                                if r.changed() {
                                    changed = true;
                                    want_model |= *ns;
                                }
                            });
                        });
                    }
                }
            });
        resp.0
            .on_hover_text(format!("Audio: {}", full_names.join(" + ")));
        if want_model {
            self.prefetch_model();
        }
        if changed && let Some(m) = mixer {
            let plan = self.preview_plan(settings);
            self.player.restart_audio(m, &plan);
        }
    }
}

/// Square Play / Pause button with a drawn icon (egui's default font lacks the glyphs).
fn play_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let size = Vec2::splat(super::CONTROL_HEIGHT);
    let resp = ui
        .add(egui::Button::new("").min_size(Vec2::new(size.x + 8.0, size.y)))
        .on_hover_text(if playing {
            "Pause (Space)"
        } else {
            "Play (Space)"
        });
    let color = ui.style().interact(&resp).fg_stroke.color;
    let c = resp.rect.center();
    let p = ui.painter();
    if playing {
        for dx in [-3.5, 3.5] {
            let bar = egui::Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(4.0, 12.0));
            p.rect_filled(bar, 1.0, color);
        }
    } else {
        let pts = vec![
            c + Vec2::new(-4.0, -6.5),
            c + Vec2::new(6.5, 0.0),
            c + Vec2::new(-4.0, 6.5),
        ];
        p.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
    }
    resp
}
