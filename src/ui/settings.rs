// SPDX-License-Identifier: Apache-2.0
//! The Settings dialog: clips folder, noise suppression strength, output prefix,
//! quality presets, audio track names, and regenerating the right-click menu.

use eframe::egui::{self, RichText};

use crate::settings::{Denoise, Preset, PresetKind, Settings, TrackCfg, fmt_num};

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
    message: Option<String>,
    offer_thunar_restart: bool,
}

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
        let rows = draft
            .presets
            .iter()
            .map(|p| Row {
                max_mb: p.max_mb.map(fmt_num).unwrap_or_default(),
                auto_suffix: false,
            })
            .collect();
        Self {
            draft,
            rows,
            applied: false,
            message: None,
            offer_thunar_restart: false,
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

    #[cfg(target_os = "linux")]
    fn regenerate_thunar(&mut self) {
        let s = self.result();
        let res = s
            .save()
            .map_err(anyhow::Error::from)
            .and_then(|_| crate::thunar::write_actions(&s, &std::env::current_exe()?));
        match res {
            Ok(uca) => {
                self.applied = true;
                let n = s.presets.iter().filter(|p| !p.is_original()).count();
                self.message = Some(format!(
                    "Saved settings and wrote {n} preset entr{} plus \u{201c}Open in Clipman\u{2026}\u{201d} \
                     to {} (previous version: uca.xml.bak). Thunar shows the new menu after it restarts.",
                    if n == 1 { "y" } else { "ies" },
                    uca.display()
                ));
                self.offer_thunar_restart = true;
            }
            Err(e) => self.message = Some(format!("Couldn't update Thunar: {e:#}")),
        }
    }

    /// Draw the dialog. Returns Some when it's closed.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Outcome> {
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
            if ui.button("Choose…").clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_directory(self.draft.clips_folder())
                    .pick_folder()
                {
                    self.draft.clips_folder = Some(dir);
                }
            if self.draft.clips_folder.is_some() && ui.button("Use default").clicked() {
                self.draft.clips_folder = None;
            }
        });
        ui.add_space(8.0);

        // noise suppression
        ui.label(RichText::new("Noise suppression strength").strong());
        ui.horizontal(|ui| {
            ui.label("Subtle");
            ui.add(egui::Slider::new(&mut self.draft.denoise_strength, 0..=100).suffix("%"));
            ui.label("Full");
        });
        ui.add_space(8.0);

        // presets
        ui.label(RichText::new("Quality presets").strong());
        ui.horizontal(|ui| {
            ui.label("File name prefix:");
            ui.add_sized(
                [140.0, 20.0],
                egui::TextEdit::singleline(&mut self.draft.output_prefix).hint_text("none"),
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
                        [230.0, 20.0],
                        egui::TextEdit::singleline(&mut p.name).hint_text(hint),
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
                            [90.0, 20.0],
                            egui::TextEdit::singleline(&mut row.max_mb).hint_text("no limit"),
                        );
                        if row.auto_suffix {
                            p.suffix = format!("_{}", p.resolution());
                        }
                    }
                    if ui
                        .add_sized(
                            [110.0, 20.0],
                            egui::TextEdit::singleline(&mut p.suffix).hint_text("none"),
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
            if ui.button("Add original-quality preset").on_hover_text("Just trims: copies the video untouched").clicked() {
                self.add_preset(PresetKind::Original);
            }
            #[cfg(target_os = "linux")]
            if ui
                .button("Regenerate right-click menu")
                .on_hover_text("Saves these settings, then rewrites Thunar's \u{201c}Clipman\u{201d} submenu: one entry per compress preset")
                .clicked()
            {
                self.regenerate_thunar();
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
                ui.add_sized([220.0, 20.0], egui::TextEdit::singleline(&mut t.label).hint_text(name.as_str()));
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
