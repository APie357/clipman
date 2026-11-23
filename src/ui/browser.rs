// SPDX-License-Identifier: Apache-2.0
//! The clip browser: a grid of the clips in the folder chosen in Settings, newest
//! first, with thumbnails generated in the background and cached on disk.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use eframe::egui::{self, Color32, Vec2};

use crate::plugin::{BrowserFilter, ClipAction, Host};
use crate::settings::{Settings, cache_dir};

const EXTENSIONS: [&str; 4] = super::VIDEO_EXTENSIONS;
/// Width of the cached thumbnails: the biggest cards, with some to spare.
const THUMB_PX: u32 = 512;
/// Card widths the size slider offers.
pub const CARD_MIN: f32 = 160.0;
pub const CARD_MAX: f32 = 480.0;
/// Space between cards.
const GAP: f32 = 12.0;
/// Below the thumbnail: name and date lines.
const CARD_TEXT_H: f32 = 46.0;
const THUMB_WORKERS: usize = 2;
/// Plugin badge callbacks run per clip; spread them over frames.
const BADGES_PER_FRAME: usize = 8;

pub enum Action {
    None,
    Open(PathBuf),
    /// A plugin action from a card's right-click menu.
    Plugin(ClipAction, PathBuf),
    /// The card size slider was let go at this width: save it.
    CardSize(u32),
}

enum CardClick {
    None,
    Open,
    Plugin(ClipAction),
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
    /// Plugin badges per clip, asked for once per refresh.
    badges: HashMap<PathBuf, Vec<String>>,
    /// The list was re-read and plugins haven't been told yet.
    refreshed: bool,
    /// Card (thumbnail) width.
    card_w: f32,
    /// Filters plugins added, asked for again when `Host::changes` moves.
    filters: Vec<BrowserFilter>,
    /// Chosen option per plugin filter, by (plugin id, filter id); none = any.
    filter_sel: HashMap<(String, String), String>,
    /// Whether each clip passes the chosen plugin filters.
    passes: HashMap<PathBuf, bool>,
    seen_changes: Option<u64>,
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
            badges: HashMap::new(),
            refreshed: false,
            card_w: (settings.card_size as f32).clamp(CARD_MIN, CARD_MAX),
            filters: Vec::new(),
            filter_sel: HashMap::new(),
            passes: HashMap::new(),
            seen_changes: None,
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
        self.badges.clear();
        self.passes.clear();
        self.refreshed = true;
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

    pub fn ui(&mut self, ctx: &egui::Context, plugins: &mut Host) -> Action {
        self.start_workers(ctx);
        self.sync_plugins(plugins);
        let actions = plugins.clip_actions();
        while let Ok((path, img)) = self.thumb_rx.try_recv() {
            match img {
                Some(img) => {
                    // mipmaps: small cards show the 512 px thumbnail scaled well down
                    let opts = egui::TextureOptions::LINEAR
                        .with_mipmap_mode(Some(egui::TextureFilter::Linear));
                    let tex = ctx.load_texture(path.to_string_lossy(), img, opts);
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
                // controls from the right; the heading and folder get what's left
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.refresh();
                    }
                    if let Some(r) = crate::update::offer() {
                        if ui
                            .small_button("✖")
                            .on_hover_text("Don\u{2019}t show this version again")
                            .clicked()
                        {
                            crate::update::skip(&r.version);
                        }
                        ui.hyperlink_to(format!("Clipman {} is available", r.version), &r.url)
                            .on_hover_text(format!(
                                "You have {}. Opens the release page.",
                                crate::update::CURRENT
                            ));
                    } else if crate::update::status() == crate::update::Status::Checking {
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(500));
                    }
                    ui.add_sized(
                        [200.0, super::CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Filter")
                            .vertical_align(egui::Align::Center),
                    );
                    if self.plugin_filter_combos(ui) {
                        self.passes.clear();
                    }
                    let size = ui
                        .add(
                            egui::Slider::new(&mut self.card_w, CARD_MIN..=CARD_MAX)
                                .show_value(false),
                        )
                        .on_hover_text("Card size");
                    if size.drag_stopped() || (size.changed() && !size.dragged()) {
                        action = Action::CardSize(self.card_w.round() as u32);
                    }
                    ui.weak("Size");
                    ui.add_space(8.0);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.heading("Clips");
                        let folder = self.folder.display().to_string();
                        ui.add(egui::Label::new(egui::RichText::new(&folder).weak()).truncate())
                            .on_hover_text(folder);
                    });
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
            let mut shown: Vec<&Clip> = self
                .clips
                .iter()
                .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
                .collect();
            if !self.filter_sel.is_empty() {
                for c in &shown {
                    if !self.passes.contains_key(&c.path) {
                        let ok = self.filter_sel.iter().all(|((pid, fid), opt)| {
                            plugins.filter_matches(pid, fid, &c.path, opt)
                        });
                        self.passes.insert(c.path.clone(), ok);
                    }
                }
                shown.retain(|c| self.passes.get(&c.path).copied().unwrap_or(true));
            }
            if plugins.has_badges() {
                let missing: Vec<PathBuf> = shown
                    .iter()
                    .filter(|c| !self.badges.contains_key(&c.path))
                    .take(BADGES_PER_FRAME)
                    .map(|c| c.path.clone())
                    .collect();
                if !missing.is_empty() {
                    ui.ctx().request_repaint();
                }
                for p in missing {
                    let b = plugins.badges(&p);
                    self.badges.insert(p, b);
                }
            }
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
                    ui.spacing_mut().item_spacing = Vec2::new(GAP, GAP);
                    let w = self.card_w;
                    let card = Vec2::new(w, self.thumb_h() + CARD_TEXT_H);
                    let avail = ui.available_width();
                    let cols = (((avail + GAP) / (w + GAP)).floor() as usize).max(1);
                    let used = cols as f32 * w + (cols - 1) as f32 * GAP;
                    // centred as a grid: a short last row stays in its columns
                    let pad = ((avail - used) / 2.0).max(0.0);
                    for row in shown.chunks(cols) {
                        let (row_rect, _) =
                            ui.allocate_exact_size(Vec2::new(avail, card.y), egui::Sense::hover());
                        if !ui.is_rect_visible(row_rect) {
                            continue;
                        }
                        for (i, c) in row.iter().enumerate() {
                            let min = row_rect.min + Vec2::new(pad + i as f32 * (w + GAP), 0.0);
                            let rect = egui::Rect::from_min_size(min, card);
                            match self.card(ui, rect, c, &actions) {
                                CardClick::None => {}
                                CardClick::Open => action = Action::Open(c.path.clone()),
                                CardClick::Plugin(a) => action = Action::Plugin(a, c.path.clone()),
                            }
                        }
                    }
                });
        });
        action
    }

    fn thumb_h(&self) -> f32 {
        (self.card_w * 9.0 / 16.0).round()
    }

    /// Tell plugins about a new listing, and ask them again for badges and filters when
    /// they (or their data) changed.
    fn sync_plugins(&mut self, plugins: &mut Host) {
        let refreshed = std::mem::take(&mut self.refreshed);
        let paths: Vec<PathBuf> = self.clips.iter().map(|c| c.path.clone()).collect();
        if refreshed {
            plugins.browser_refresh(&paths);
        }
        if !refreshed && self.seen_changes == Some(plugins.changes()) {
            return;
        }
        self.badges.clear();
        self.passes.clear();
        self.filters = plugins.browser_filters(&paths);
        // keep choices whose option is still there
        let filters = &self.filters;
        self.filter_sel.retain(|(pid, fid), opt| {
            filters
                .iter()
                .any(|f| &f.plugin_id == pid && &f.id == fid && f.options.contains(opt))
        });
        self.seen_changes = Some(plugins.changes());
    }

    /// A dropdown per plugin filter (in a right-to-left layout). True if a choice changed.
    fn plugin_filter_combos(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        for f in self.filters.iter().rev() {
            let key = (f.plugin_id.clone(), f.id.clone());
            let mut sel = self.filter_sel.get(&key).cloned();
            let text = match &sel {
                Some(o) => format!("{}: {}", f.label, elide(o, 24)),
                None => format!("{}: any", f.label),
            };
            egui::ComboBox::from_id_salt(("plugin-filter", &f.plugin_id, &f.id))
                .selected_text(text)
                .show_ui(ui, |ui| {
                    super::menu_style(ui);
                    changed |= ui.selectable_value(&mut sel, None, "Any").changed();
                    for o in &f.options {
                        changed |= ui
                            .selectable_value(&mut sel, Some(o.clone()), o.as_str())
                            .changed();
                    }
                    if f.options.is_empty() {
                        ui.weak("Nothing to choose from");
                    }
                });
            match sel {
                Some(o) => self.filter_sel.insert(key, o),
                None => self.filter_sel.remove(&key),
            };
        }
        changed
    }

    /// One clip card in `rect`: click opens it, right-click shows plugin actions.
    fn card(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        c: &Clip,
        actions: &[ClipAction],
    ) -> CardClick {
        let resp = ui.interact(rect, ui.id().with(&c.path), egui::Sense::click());
        let p = ui.painter();
        let (tw, th) = (self.card_w, self.thumb_h());
        let img = egui::Rect::from_min_size(rect.min, Vec2::new(tw, th));
        p.rect_filled(img, 6.0, Color32::from_gray(20));
        match self.thumbs.get(&c.path) {
            Some(t) => {
                let s = t.size_vec2();
                let fit = (tw / s.x).min(th / s.y);
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
        let name = elide(&c.name, (tw / 7.5) as usize);
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
        // plugin badges, top left of the thumbnail
        let mut x = img.left() + 6.0;
        for b in self.badges.get(&c.path).into_iter().flatten() {
            let galley = p.layout_no_wrap(
                elide(b, 24),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
            let pill = egui::Rect::from_min_size(
                egui::pos2(x, img.top() + 6.0),
                galley.size() + Vec2::new(10.0, 4.0),
            );
            if pill.right() > img.right() - 4.0 {
                break;
            }
            p.rect_filled(pill, 4.0, Color32::from_black_alpha(170));
            p.galley(pill.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
            x = pill.right() + 4.0;
        }
        let mut click = CardClick::None;
        resp.context_menu(|ui| {
            super::menu_style(ui);
            if ui.button("Open").clicked() {
                click = CardClick::Open;
            }
            if !actions.is_empty() {
                ui.separator();
            }
            for a in actions {
                if ui.button(a.label.as_str()).clicked() {
                    click = CardClick::Plugin(a.clone());
                }
            }
        });
        if resp.on_hover_text(&c.name).clicked() {
            click = CardClick::Open;
        }
        click
    }
}

fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
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
                .args(["-vf", &format!("scale={THUMB_PX}:-2")]);
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
