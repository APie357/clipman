// SPDX-License-Identifier: Apache-2.0
//! The Settings dialog: clips folder, noise suppression strength, video encoder, output prefix,
//! quality presets, audio track names, and regenerating the right-click menu.

use eframe::egui::{self, RichText};

use crate::settings::{Denoise, Encoder, Preset, PresetKind, Settings, TrackCfg, fmt_num};

pub enum Outcome {
    /// Saved with the Save button (or Regenerate) — use these settings.
    Saved(Settings),
    Cancelled,
}

/// Per-preset editing state that isn't part of the saved settings.
struct Row {
    max_mb: String,
    /// New presets' suffix follows their resolution until it's edited.
    auto_suffix: bool,
}

pub struct SettingsUi {
    draft: Settings,
    rows: Vec<Row>,
    /// Settings were already written (by Regenerate) even if the dialog is cancelled.
    applied: bool,
    /// Whether update checks were on when the dialog opened (so a check is running).
    settings_check_updates: bool,
    message: Option<String>,
    offer_thunar_restart: bool,
    /// A folder picker that's open (on its own thread) and which setting it's for.
    picker: Option<(Folder, super::PendingDialog)>,
}

#[derive(Clone, Copy)]
enum Folder {
    Clips,
    Exports,
}

const ENCODER_CHOICES: [(Encoder, &str, &str); 3] = [
    (
        Encoder::Auto,
        "Auto",
        "GPU for presets without a size limit (several times faster, somewhat larger files); \
         CPU when fitting a size limit, where it looks clearly better at the same size",
    ),
    (
        Encoder::Gpu,
        "GPU",
        "Always the GPU: fastest, but softer when fitting a size limit",
    ),
    (Encoder::Cpu, "CPU (x264)", "Always x264: best quality per MB, slowest"),
];

const DENOISE_CHOICES: [(Denoise, &str); 3] = [
    (Denoise::None, "Not offered"),
    (Denoise::Off, "Offered (off)"),
    (Denoise::On, "On by default"),
];

impl SettingsUi {
    /// `clip_tracks`: track names of the clip that's open, added to the table if new.
    pub fn new(settings: &Settings, clip_tracks: &[Option<String>]) -> Self {
        let mut draft = settings.clone();
        for name in clip_tracks.iter().flatten() {
            if !draft.tracks.contains_key(name) {
                draft.tracks.insert(
                    name.clone(),
                    TrackCfg {
                        label: name.clone(),
                        ..Default::default()
                    },
                );
            }
        }
        // find the GPU encoder in the background so the dialog can show it
        if crate::gpu::detected().is_none() {
            std::thread::spawn(crate::gpu::detect);
        }
        let rows = draft
            .presets
            .iter()
            .map(|p| Row {
                max_mb: p.max_mb.map(fmt_num).unwrap_or_default(),
                auto_suffix: false,
            })
            .collect();
        Self {
            settings_check_updates: settings.check_updates,
            draft,
            rows,
            applied: false,
            message: None,
            offer_thunar_restart: false,
            picker: None,
        }
    }

    fn result(&self) -> Settings {
        let mut s = self.draft.clone();
        for (p, row) in s.presets.iter_mut().zip(&self.rows) {
            p.max_mb = row
                .max_mb
                .trim()
                .replace(',', ".")
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0);
        }
        s.output_prefix = s.output_prefix.trim().replace(['/', '\\'], "_");
        s
    }

    fn add_preset(&mut self, kind: PresetKind) {
        let id = self.result().new_preset_id();
        let original = kind == PresetKind::Original;
        self.draft.presets.push(Preset {
            id,
            kind,
            name: String::new(),
            height: if original { 0 } else { 1080 },
            fps: if original { 0 } else { 60 },
            max_mb: if original {
                None
            } else {
                Some(crate::settings::DEFAULT_MAX_MB)
            },
            suffix: String::new(),
        });
        self.rows.push(Row {
            max_mb: if original {
                String::new()
            } else {
                fmt_num(crate::settings::DEFAULT_MAX_MB)
            },
            auto_suffix: !original,
        });
    }

    fn regenerate_menu(&mut self) {
        let s = self.result();
        let res = s
            .save()
            .map_err(anyhow::Error::from)
            .and_then(|_| crate::menu::write(&s, &std::env::current_exe()?));
        match res {
            Ok(place) => {
                self.applied = true;
                let n = s.presets.iter().filter(|p| !p.is_original()).count();
                let entries = format!(
                    "{n} preset entr{} plus \u{201c}Open in Clipman\u{2026}\u{201d}",
                    if n == 1 { "y" } else { "ies" }
                );
                self.message = Some(if cfg!(windows) {
                    format!(
                        "Saved settings and wrote {entries} to the Explorer menu of .mp4, .mkv \
                         and .mov files (on Windows 11: under \u{201c}Show more options\u{201d})."
                    )
                } else {
                    format!(
                        "Saved settings and wrote {entries} to {place} (previous version: \
                         uca.xml.bak). Thunar shows the new menu after it restarts."
                    )
                });
                self.offer_thunar_restart = cfg!(target_os = "linux");
            }
            Err(e) => self.message = Some(format!("Couldn't update the right-click menu: {e:#}")),
        }
    }

    /// Draw the dialog. Returns Some when it's closed.
    fn pick_folder(&mut self, ctx: &egui::Context, which: Folder, start: std::path::PathBuf) {
        if self.picker.is_none() {
            let d = super::PendingDialog::spawn(ctx, move || {
                rfd::FileDialog::new().set_directory(start).pick_folder()
            });
            self.picker = Some((which, d));
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<Outcome> {
        if let Some(result) = self.picker.as_ref().and_then(|(_, d)| d.poll()) {
            let which = self.picker.take().map(|(w, _)| w);
            match (which, result) {
                (Some(Folder::Clips), Some(dir)) => self.draft.clips_folder = Some(dir),
                (Some(Folder::Exports), Some(dir)) => self.draft.export_folder = Some(dir),
                _ => {}
            }
        } else if self.picker.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut outcome = None;
        egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(860.0_f32.min(ctx.content_rect().width() - 40.0));
            ui.heading("Settings");
            ui.separator();
            let max_h = (ctx.content_rect().height() - 140.0).max(200.0);
            egui::ScrollArea::vertical()
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| self.contents(ui));
            ui.separator();
            if let Some(msg) = &self.message {
                ui.label(msg);
                if self.offer_thunar_restart {
                    ui.horizontal(|ui| {
                        if ui
                            .button("Restart Thunar now")
                            .on_hover_text("Closes any open Thunar windows")
                            .clicked()
                        {
                            let _ = crate::media::quiet_command("thunar").arg("-q").status();
                            self.offer_thunar_restart = false;
                        }
                        if ui.button("Later").clicked() {
                            self.offer_thunar_restart = false;
                        }
                    });
                }
                ui.separator();
            }
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    let s = self.result();
                    match s.save() {
                        Ok(()) => outcome = Some(Outcome::Saved(s)),
                        Err(e) => self.message = Some(format!("Couldn't save settings: {e}")),
                    }
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    // Regenerate already saved: keep what was written
                    outcome = Some(if self.applied {
                        Outcome::Saved(Settings::load())
                    } else {
                        Outcome::Cancelled
                    });
                }
            });
        });
        outcome
    }

    fn contents(&mut self, ui: &mut egui::Ui) {
        // clips folder
        ui.label(RichText::new("Clips folder").strong());
        ui.horizontal(|ui| {
            ui.label(self.draft.clips_folder().display().to_string());
            if ui
                .add_enabled(self.picker.is_none(), egui::Button::new("Choose…"))
                .clicked()
            {
                self.pick_folder(ui.ctx(), Folder::Clips, self.draft.clips_folder());
            }
            if self.draft.clips_folder.is_some() && ui.button("Use default").clicked() {
                self.draft.clips_folder = None;
            }
        });
        ui.add_space(8.0);

        // export folder
        ui.label(RichText::new("Export folder").strong());
        ui.horizontal(|ui| {
            let folder = self.draft.export_folder();
            ui.label(folder.display().to_string());
            if ui
                .add_enabled(self.picker.is_none(), egui::Button::new("Choose…"))
                .clicked()
            {
                // the picker needs an existing folder to start in
                let start = if folder.is_dir() {
                    folder
                } else {
                    self.draft.clips_folder()
                };
                self.pick_folder(ui.ctx(), Folder::Exports, start);
            }
            if self.draft.export_folder.is_some() && ui.button("Use default").clicked() {
                self.draft.export_folder = None;
            }
        });
        if self.draft.exports_in_clips_folder() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "This is the clips folder, so saved clips will show in the clip browser. \
                 Pick a separate folder to keep them out of it.",
            );
        } else {
            ui.weak("Saved clips go here (created when needed). They don\u{2019}t show in the clip browser.");
        }
        ui.add_space(8.0);

        // noise suppression
        ui.label(RichText::new("Noise suppression strength").strong());
        ui.horizontal(|ui| {
            ui.label("Subtle");
            ui.add(egui::Slider::new(&mut self.draft.denoise_strength, 0..=100).suffix("%"));
            ui.label("Full");
        });
        ui.add_space(8.0);

        // encoder
        ui.label(RichText::new("Video encoder").strong());
        ui.horizontal(|ui| {
            let (_, text, _) = ENCODER_CHOICES
                .iter()
                .find(|c| c.0 == self.draft.encoder)
                .unwrap();
            egui::ComboBox::from_id_salt("encoder")
                .selected_text(*text)
                .show_ui(ui, |ui| {
                    for (v, text, hint) in ENCODER_CHOICES {
                        ui.selectable_value(&mut self.draft.encoder, v, text)
                            .on_hover_text(hint);
                    }
                });
            match crate::gpu::detected() {
                None => {
                    ui.weak("Looking for a GPU encoder…");
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(200));
                }
                Some(Some(hw)) => {
                    ui.weak(format!("GPU: {}", hw.describe()));
                }
                Some(None) if self.draft.encoder == Encoder::Cpu => {
                    ui.weak("No working GPU encoder found");
                }
                Some(None) => {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "No working GPU encoder found: renders use the CPU",
                    );
                }
            }
        });
        let (_, _, hint) = ENCODER_CHOICES
            .iter()
            .find(|c| c.0 == self.draft.encoder)
            .unwrap();
        ui.weak(*hint);
        ui.checkbox(&mut self.draft.gpu_playback, "Decode playback on the GPU")
            .on_hover_text(
                "Uses about half the CPU while a clip plays, but playback takes ~80 ms longer \
                 to start. Seeking and scrubbing stay on the CPU, which is faster for them.",
            );
        ui.add_space(8.0);

        // updates
        if crate::update::ENABLED {
            ui.label(RichText::new("Updates").strong());
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.draft.check_updates, "Check for updates")
                    .on_hover_text("Once a day, asks GitHub for the latest release. Nothing is downloaded or installed.");
                use crate::update::Status;
                match crate::update::status() {
                    _ if !self.draft.check_updates => {}
                    Status::Checking if self.settings_check_updates => {
                        ui.weak("Checking…");
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                    }
                    Status::Checking | Status::Disabled => {}
                    Status::UpToDate => {
                        ui.weak(format!("Clipman {} is up to date", crate::update::CURRENT));
                    }
                    Status::Available(r) => {
                        ui.hyperlink_to(
                            format!("Clipman {} is available (you have {})", r.version, crate::update::CURRENT),
                            &r.url,
                        );
                    }
                    Status::Failed(e) => {
                        ui.weak(format!("Couldn\u{2019}t check: {e}"));
                    }
                }
            });
            ui.add_space(8.0);
        }

        // presets
        ui.label(RichText::new("Quality presets").strong());
        ui.horizontal(|ui| {
            ui.label("File name prefix:");
            ui.add_sized(
                [140.0, super::CONTROL_HEIGHT],
                egui::TextEdit::singleline(&mut self.draft.output_prefix)
                    .vertical_align(egui::Align::Center)
                    .hint_text("none"),
            );
            ui.weak(format!(
                "Files are named {}<clip name><suffix>.mp4",
                self.draft.output_prefix.trim()
            ));
        });
        let mut remove = None;
        egui::Grid::new("presets")
            .striped(true)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                for h in [
                    "Name",
                    "Max height",
                    "Max fps",
                    "Max size (MB)",
                    "File suffix",
                    "",
                ] {
                    ui.strong(h);
                }
                ui.end_row();
                for (i, (p, row)) in self
                    .draft
                    .presets
                    .iter_mut()
                    .zip(&mut self.rows)
                    .enumerate()
                {
                    let hint = p.label();
                    ui.add_sized(
                        [230.0, super::CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut p.name)
                            .vertical_align(egui::Align::Center)
                            .hint_text(hint),
                    );
                    if p.is_original() {
                        for _ in 0..3 {
                            ui.weak("—");
                        }
                    } else {
                        ui.add(
                            egui::DragValue::new(&mut p.height)
                                .range(144..=4320)
                                .suffix("p"),
                        );
                        ui.add(egui::DragValue::new(&mut p.fps).range(1..=240));
                        ui.add_sized(
                            [90.0, super::CONTROL_HEIGHT],
                            egui::TextEdit::singleline(&mut row.max_mb)
                                .vertical_align(egui::Align::Center)
                                .hint_text("no limit"),
                        );
                        if row.auto_suffix {
                            p.suffix = format!("_{}", p.resolution());
                        }
                    }
                    if ui
                        .add_sized(
                            [110.0, super::CONTROL_HEIGHT],
                            egui::TextEdit::singleline(&mut p.suffix)
                                .vertical_align(egui::Align::Center)
                                .hint_text("none"),
                        )
                        .changed()
                    {
                        row.auto_suffix = false;
                    }
                    if ui.button("✖").on_hover_text("Remove this preset").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
        if let Some(i) = remove {
            self.draft.presets.remove(i);
            self.rows.remove(i);
        }
        if self.draft.presets.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "No presets: add one to be able to save clips.",
            );
        }
        ui.horizontal(|ui| {
            if ui.button("Add compress preset").clicked() {
                self.add_preset(PresetKind::Compress);
            }
            if ui.button("Add source preset").on_hover_text("Just trims: copies the video as-is, no re-encoding").clicked() {
                self.add_preset(PresetKind::Original);
            }
            if crate::menu::SUPPORTED
                && ui
                    .button("Regenerate right-click menu")
                    .on_hover_text(format!(
                        "Saves these settings, then rewrites {}\u{2019}s \u{201c}Clipman\u{201d} submenu: one entry per compress preset",
                        if cfg!(windows) { "Explorer" } else { "Thunar" }
                    ))
                    .clicked()
            {
                self.regenerate_menu();
            }
        });
        ui.weak("Leave a name blank to show it as resolution + fps. Leave Max size blank for no size limit (fixed high quality instead).");
        ui.add_space(8.0);

        // tracks
        ui.label(RichText::new("Audio tracks").strong());
        ui.weak("Matched by the name stored in the clip (set in GPU Screen Recorder). Unnamed tracks show as \u{201c}Track N\u{201d}.");
        let mut forget = None;
        egui::Grid::new("tracks").striped(true).spacing([10.0, 6.0]).show(ui, |ui| {
            for h in ["Name in clip", "Shown as", "Ticked", "Noise suppression", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for (name, t) in self.draft.tracks.iter_mut() {
                ui.label(name);
                ui.add_sized([220.0, super::CONTROL_HEIGHT], egui::TextEdit::singleline(&mut t.label).vertical_align(egui::Align::Center).hint_text(name.as_str()));
                ui.checkbox(&mut t.default, "").on_hover_text("Ticked by default in the Audio dropdown and when rendering from the right-click menu");
                egui::ComboBox::from_id_salt(("dn", name.as_str()))
                    .selected_text(DENOISE_CHOICES.iter().find(|c| c.0 == t.denoise).unwrap().1)
                    .show_ui(ui, |ui| {
                        for (v, text) in DENOISE_CHOICES {
                            ui.selectable_value(&mut t.denoise, v, text);
                        }
                    });
                if ui.button("✖").on_hover_text("Forget this name").clicked() {
                    forget = Some(name.clone());
                }
                ui.end_row();
            }
        });
        if let Some(n) = forget {
            self.draft.tracks.shift_remove(&n);
        }
    }
}
