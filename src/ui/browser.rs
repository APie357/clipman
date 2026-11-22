// SPDX-License-Identifier: Apache-2.0
//! The clip browser: a grid of the clips in the folder chosen in Settings, newest
//! first, with thumbnails generated in the background and cached on disk.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use eframe::egui::{self, Color32, Vec2};

use crate::settings::{Settings, cache_dir};

const EXTENSIONS: [&str; 4] = ["mp4", "mkv", "mov", "webm"];
const THUMB_W: f32 = 256.0;
const THUMB_H: f32 = 144.0;
const THUMB_WORKERS: usize = 2;

pub enum Action {
    None,
    Open(PathBuf),
    OpenSettings,
}

struct Clip {
    path: PathBuf,
    name: String,
    modified: SystemTime,
    size: u64,
}

pub struct Browser {
    folder: PathBuf,
    clips: Vec<Clip>,
    filter: String,
    error: Option<String>,
    thumbs: HashMap<PathBuf, egui::TextureHandle>,
    failed: std::collections::HashSet<PathBuf>,
    queue: Arc<Mutex<VecDeque<(PathBuf, PathBuf)>>>,
    thumb_rx: Receiver<(PathBuf, Option<egui::ColorImage>)>,
    thumb_tx: Sender<(PathBuf, Option<egui::ColorImage>)>,
    workers_started: bool,
}

impl Browser {
    pub fn new(settings: &Settings) -> Self {
        let (thumb_tx, thumb_rx) = mpsc::channel();
        let mut b = Self {
            folder: settings.clips_folder(),
            clips: Vec::new(),
            filter: String::new(),
            error: None,
            thumbs: HashMap::new(),
            failed: Default::default(),
            queue: Default::default(),
            thumb_rx,
            thumb_tx,
            workers_started: false,
        };
        b.refresh();
        b
    }

    pub fn set_folder(&mut self, folder: PathBuf) {
        if folder != self.folder {
            self.folder = folder;
            self.refresh();
        }
    }

    pub fn refresh(&mut self) {
        self.error = None;
        self.clips = match std::fs::read_dir(&self.folder) {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| {
                    let path = e.path();
                    let ext = path.extension()?.to_string_lossy().to_lowercase();
                    if !EXTENSIONS.contains(&ext.as_str()) {
                        return None;
                    }
                    let meta = e.metadata().ok().filter(|m| m.is_file())?;
                    Some(Clip {
                        name: path.file_name()?.to_string_lossy().into_owned(),
                        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        size: meta.len(),
                        path,
                    })
                })
                .collect(),
            Err(e) => {
                self.error = Some(format!("Couldn't open {}: {e}", self.folder.display()));
                Vec::new()
            }
        };
        self.clips.sort_by(|a, b| b.modified.cmp(&a.modified));
        // queue thumbnails, newest first
        let mut q = self.queue.lock().unwrap();
        q.clear();
        for c in &self.clips {
            if !self.thumbs.contains_key(&c.path) {
                q.push_back((c.path.clone(), thumb_cache_path(c)));
            }
        }
    }

    fn start_workers(&mut self, ctx: &egui::Context) {
        if self.workers_started {
            return;
        }
        self.workers_started = true;
        for _ in 0..THUMB_WORKERS {
            let (q, tx, ctx) = (self.queue.clone(), self.thumb_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                loop {
                    let job = q.lock().unwrap().pop_front();
                    let Some((clip, cache)) = job else {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        continue;
                    };
                    let img = load_or_make_thumb(&clip, &cache);
                    if tx.send((clip, img)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Action {
        self.start_workers(ctx);
        while let Ok((path, img)) = self.thumb_rx.try_recv() {
            match img {
                Some(img) => {
                    let tex =
                        ctx.load_texture(path.to_string_lossy(), img, egui::TextureOptions::LINEAR);
                    self.thumbs.insert(path, tex);
                }
                None => {
                    self.failed.insert(path);
                }
            }
        }
        let mut action = Action::None;
        egui::TopBottomPanel::top("browser_top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Clips");
                ui.weak(self.folder.display().to_string());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        action = Action::OpenSettings;
                    }
                    if ui.button("Refresh").clicked() {
                        self.refresh();
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Filter")
                            .desired_width(200.0),
                    );
                });
            });
            ui.add_space(2.0);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
                ui.label("Pick another folder in Settings.");
                return;
            }
            let needle = self.filter.to_lowercase();
            let shown: Vec<&Clip> = self
                .clips
                .iter()
                .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
                .collect();
            if shown.is_empty() {
                ui.weak(if self.clips.is_empty() {
                    "No clips in this folder."
                } else {
                    "Nothing matches the filter."
                });
                return;
            }
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(12.0, 12.0);
                        for c in shown {
                            if self.card(ui, c) {
                                action = Action::Open(c.path.clone());
                            }
                        }
                    });
                });
        });
        action
    }

    /// One clip card. Returns true when clicked.
    fn card(&self, ui: &mut egui::Ui, c: &Clip) -> bool {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(THUMB_W, THUMB_H + 46.0), egui::Sense::click());
        if !ui.is_rect_visible(rect) {
            return false;
        }
        let p = ui.painter();
        let img = egui::Rect::from_min_size(rect.min, Vec2::new(THUMB_W, THUMB_H));
        p.rect_filled(img, 6.0, Color32::from_gray(20));
        match self.thumbs.get(&c.path) {
            Some(t) => {
                let s = t.size_vec2();
                let fit = (THUMB_W / s.x).min(THUMB_H / s.y);
                let r = egui::Rect::from_center_size(img.center(), s * fit);
                p.image(
                    t.id(),
                    r,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                let text = if self.failed.contains(&c.path) {
                    "no preview"
                } else {
                    "…"
                };
                p.text(
                    img.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(14.0),
                    Color32::GRAY,
                );
            }
        }
        if resp.hovered() {
            p.rect_stroke(
                img,
                6.0,
                egui::Stroke::new(2.0, ui.visuals().selection.bg_fill),
                egui::StrokeKind::Outside,
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let text_color = ui.visuals().text_color();
        let name = elide(&c.name, 34);
        p.text(
            img.left_bottom() + Vec2::new(2.0, 6.0),
            egui::Align2::LEFT_TOP,
            name,
            egui::FontId::proportional(14.0),
            text_color,
        );
        p.text(
            img.left_bottom() + Vec2::new(2.0, 26.0),
            egui::Align2::LEFT_TOP,
            format!("{}  ·  {}", fmt_date(c.modified), fmt_size(c.size)),
            egui::FontId::proportional(12.0),
            ui.visuals().weak_text_color(),
        );
        resp.on_hover_text(&c.name).clicked()
    }
}

fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

fn fmt_size(b: u64) -> String {
    let mb = b as f64 / 1e6;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// "2026-11-21 16:08" in local time.
fn fmt_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = secs + local_offset_secs();
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// The local UTC offset, from the `date` command on Unix (no tz database needed);
/// UTC elsewhere.
fn local_offset_secs() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        #[cfg(unix)]
        if let Ok(out) = std::process::Command::new("date").arg("+%z").output() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.len() == 5 {
                let sign = if s.starts_with('-') { -1 } else { 1 };
                if let (Ok(h), Ok(m)) = (s[1..3].parse::<i64>(), s[3..5].parse::<i64>()) {
                    return sign * (h * 3600 + m * 60);
                }
            }
        }
        0
    })
}

fn thumb_cache_path(c: &Clip) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mtime = c
        .modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = format!("{}|{}|{}", c.path.display(), mtime, c.size);
    let hash: String = Sha256::digest(key.as_bytes())
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect();
    cache_dir().join("thumbs").join(format!("{hash}.jpg"))
}

fn load_or_make_thumb(clip: &Path, cache: &Path) -> Option<egui::ColorImage> {
    if !cache.exists() {
        let _ = std::fs::create_dir_all(cache.parent()?);
        // a frame 1 s in (clips often start on a black frame); fall back to the start
        for at in ["1", "0"] {
            let mut cmd = crate::render::ffmpeg();
            cmd.seek(at)
                .input(clip)
                .frames(1)
                .no_audio()
                .args(["-vf", &format!("scale={}:-2", THUMB_W as u32 * 2)]);
            cmd.args(["-q:v", "4"]).overwrite().output(cache);
            if let Ok(mut child) = cmd.spawn() {
                if let Ok(iter) = child.iter() {
                    for _ in iter {}
                }
                let _ = child.wait();
            }
            if cache.exists() {
                break;
            }
        }
    }
    let img = image::open(cache).ok()?.to_rgb8();
    Some(egui::ColorImage::from_rgb(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    ))
}
