// SPDX-License-Identifier: Apache-2.0
//! The Plugins dialog: a manager tab listing the installed plugins (turn them on or off),
//! and one tab per plugin with its settings.

use std::path::PathBuf;

use eframe::egui::{self, RichText};
use indexmap::IndexMap;

use crate::plugin::{PluginInfo, SettingDef, SettingKind};
use crate::settings::PluginCfg;

/// What the dialog wants the app to do.
pub enum Action {
    /// Save these plugin settings and reload what changed; keep the dialog open.
    Apply(IndexMap<String, PluginCfg>),
    /// Save and close.
    Ok(IndexMap<String, PluginCfg>),
    /// Load the plugins again from disk (picks up edited or new plugins).
    Reload,
    Cancel,
}

#[derive(Clone, PartialEq)]
enum Tab {
    Manager,
    /// A plugin's settings, by id.
    Plugin(String),
}

pub struct PluginsUi {
    infos: Vec<PluginInfo>,
    /// The plugin part of the settings, as edited.
    draft: IndexMap<String, PluginCfg>,
    /// As last saved (to tell whether there's anything to apply).
    saved: IndexMap<String, PluginCfg>,
    tab: Tab,
    /// A folder picker that's open, for (plugin id, setting key).
    picker: Option<((String, String), super::PendingDialog)>,
    /// Where folder pickers start when the setting is empty.
    clips_folder: PathBuf,
    dir: PathBuf,
    pub message: Option<String>,
}

impl PluginsUi {
    pub fn new(
        infos: Vec<PluginInfo>,
        plugins: &IndexMap<String, PluginCfg>,
        clips_folder: PathBuf,
        dir: PathBuf,
    ) -> Self {
        Self {
            infos,
            draft: plugins.clone(),
            saved: plugins.clone(),
            tab: Tab::Manager,
            picker: None,
            clips_folder,
            dir,
            message: None,
        }
    }

    /// After Apply / Reload: what's loaded now, and what was saved.
    pub fn applied(&mut self, infos: Vec<PluginInfo>, plugins: &IndexMap<String, PluginCfg>) {
        self.infos = infos;
        self.saved = plugins.clone();
        // keep edits that weren't applied (Reload doesn't save)
        for (id, cfg) in plugins {
            self.draft.entry(id.clone()).or_insert_with(|| cfg.clone());
        }
        if let Tab::Plugin(id) = &self.tab
            && !self.has_tab(id)
        {
            self.tab = Tab::Manager;
        }
    }

    fn enabled(&self, id: &str) -> bool {
        self.draft.get(id).is_none_or(|c| c.enabled)
    }

    /// Plugins whose settings get a tab: loaded, and declaring settings.
    fn has_tab(&self, id: &str) -> bool {
        self.infos
            .iter()
            .any(|i| i.id == id && i.enabled && !i.settings.is_empty())
    }

    fn dirty(&self) -> bool {
        // entries that only differ by existing or not (default = enabled, no values)
        let norm = |m: &IndexMap<String, PluginCfg>, id: &str| {
            m.get(id).cloned().unwrap_or_default()
        };
        self.draft
            .keys()
            .chain(self.saved.keys())
            .any(|id| norm(&self.draft, id) != norm(&self.saved, id))
    }

    /// Draw the dialog. Returns what to do once a button was used.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Action> {
        if let Some(result) = self.picker.as_ref().and_then(|(_, d)| d.poll()) {
            if let (Some(((id, key), _)), Some(dir)) = (self.picker.take(), result) {
                let v = dir.to_string_lossy().into_owned().into();
                self.draft.entry(id).or_default().values.insert(key, v);
            }
        } else if self.picker.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut action = None;
        egui::Modal::new(egui::Id::new("plugins")).show(ctx, |ui| {
            ui.set_width(720.0_f32.min(ctx.content_rect().width() - 40.0));
            ui.heading("Plugins");
            ui.add_space(4.0);
            self.tabs(ui);
            ui.separator();
            let max_h = (ctx.content_rect().height() - 180.0).max(160.0);
            egui::ScrollArea::vertical()
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| match self.tab.clone() {
                    Tab::Manager => self.manager(ui, &mut action),
                    Tab::Plugin(id) => self.plugin_tab(ui, &id),
                });
            ui.separator();
            if let Some(m) = &self.message {
                ui.label(m);
                ui.separator();
            }
            ui.horizontal(|ui| {
                let dirty = self.dirty();
                if ui.button("OK").clicked() {
                    action = Some(Action::Ok(self.draft.clone()));
                }
                if ui
                    .add_enabled(dirty, egui::Button::new("Apply"))
                    .on_hover_text("Save, and load or unload plugins that were turned on or off")
                    .clicked()
                {
                    action = Some(Action::Apply(self.draft.clone()));
                }
                if ui.button("Cancel").clicked()
                    || (self.picker.is_none() && ui.input(|i| i.key_pressed(egui::Key::Escape)))
                {
                    action = Some(Action::Cancel);
                }
            });
        });
        action
    }

    fn tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Manager, "Installed");
            for i in &self.infos {
                if i.enabled && !i.settings.is_empty() {
                    ui.selectable_value(&mut self.tab, Tab::Plugin(i.id.clone()), i.name.as_str());
                }
            }
        });
    }

    fn manager(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.horizontal(|ui| {
            ui.weak(self.dir.display().to_string());
            if ui.button("Open folder").clicked() {
                let _ = std::fs::create_dir_all(&self.dir);
                super::open_path(&self.dir);
            }
            if ui
                .button("Reload")
                .on_hover_text("Load the plugins again (after editing or adding one)")
                .clicked()
            {
                *action = Some(Action::Reload);
            }
        });
        ui.weak(
            "Plugins run with your permissions, like a config file: only add ones you trust.",
        );
        ui.add_space(6.0);
        if self.infos.is_empty() {
            ui.label("No plugins installed. Each plugin is a folder with an init.lua in it; see docs/plugins.md.");
            return;
        }
        let infos = self.infos.clone();
        for (n, p) in infos.iter().enumerate() {
            if n > 0 {
                ui.separator();
            }
            ui.horizontal(|ui| {
                let mut on = self.enabled(&p.id);
                let title = if p.version.is_empty() {
                    p.name.clone()
                } else {
                    format!("{} {}", p.name, p.version)
                };
                if ui.checkbox(&mut on, RichText::new(title).strong()).changed() {
                    self.draft.entry(p.id.clone()).or_default().enabled = on;
                }
                if p.name != p.id {
                    ui.weak(format!("({})", p.id));
                }
                if self.has_tab(&p.id)
                    && ui.small_button("Settings").clicked()
                {
                    self.tab = Tab::Plugin(p.id.clone());
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(26.0); // under the name, past the checkbox
                ui.vertical(|ui| {
                    if !p.description.is_empty() {
                        ui.label(&p.description);
                    }
                    if let Some(e) = &p.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    let on = self.enabled(&p.id);
                    if on != p.enabled {
                        ui.weak(if on {
                            "Loads when you apply."
                        } else {
                            "Unloads when you apply."
                        });
                    }
                });
            });
        }
    }

    fn plugin_tab(&mut self, ui: &mut egui::Ui, id: &str) {
        let Some(p) = self.infos.iter().find(|i| i.id == id).cloned() else {
            return;
        };
        if !p.description.is_empty() {
            ui.label(&p.description);
        }
        if let Some(e) = &p.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if !self.enabled(id) {
            ui.weak("Turned off: these settings are kept for when it's on again.");
        }
        ui.add_space(6.0);
        egui::Grid::new(("plugin-settings", id))
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                for d in &p.settings {
                    self.setting(ui, id, d);
                    ui.end_row();
                }
            });
    }

    /// One row: label, editor, help.
    fn setting(&mut self, ui: &mut egui::Ui, id: &str, d: &SettingDef) {
        use serde_json::Value;
        let current = self
            .draft
            .get(id)
            .and_then(|c| c.values.get(&d.key))
            .filter(|v| !v.is_null())
            .unwrap_or(&d.default)
            .clone();
        ui.label(&d.label);
        let mut new: Option<Value> = None;
        match &d.kind {
            SettingKind::String => {
                let mut s = current.as_str().unwrap_or_default().to_string();
                if ui
                    .add_sized(
                        [280.0, super::CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut s).vertical_align(egui::Align::Center),
                    )
                    .changed()
                {
                    new = Some(s.into());
                }
            }
            SettingKind::Bool => {
                let mut b = current.as_bool().unwrap_or(false);
                if ui.checkbox(&mut b, "").changed() {
                    new = Some(b.into());
                }
            }
            SettingKind::Number { min, max } => {
                let mut n = current.as_f64().unwrap_or(0.0);
                let mut dv = egui::DragValue::new(&mut n);
                if min.is_some() || max.is_some() {
                    dv = dv.range(min.unwrap_or(f64::MIN)..=max.unwrap_or(f64::MAX));
                }
                if ui.add(dv).changed() {
                    new = Some(n.into());
                }
            }
            SettingKind::Choice(opts) => {
                let mut sel = current.as_str().unwrap_or_default().to_string();
                egui::ComboBox::from_id_salt(("plugin-choice", id, d.key.as_str()))
                    .selected_text(sel.clone())
                    .show_ui(ui, |ui| {
                        for o in opts {
                            if ui.selectable_value(&mut sel, o.clone(), o.as_str()).changed() {
                                new = Some(sel.clone().into());
                            }
                        }
                    });
            }
            SettingKind::Folder => {
                ui.horizontal(|ui| {
                    let s = current.as_str().unwrap_or_default();
                    if s.is_empty() {
                        ui.weak("not set");
                    } else {
                        ui.label(s);
                    }
                    if ui
                        .add_enabled(self.picker.is_none(), egui::Button::new("Choose…"))
                        .clicked()
                    {
                        let start = if std::path::Path::new(s).is_dir() {
                            PathBuf::from(s)
                        } else {
                            self.clips_folder.clone()
                        };
                        let dialog = super::PendingDialog::spawn(ui.ctx(), move || {
                            rfd::FileDialog::new().set_directory(start).pick_folder()
                        });
                        self.picker = Some(((id.to_string(), d.key.clone()), dialog));
                    }
                    if !s.is_empty() && ui.button("Clear").clicked() {
                        new = Some("".into());
                    }
                });
            }
        }
        if d.help.is_empty() {
            ui.label("");
        } else {
            ui.weak(&d.help);
        }
        if let Some(v) = new {
            self.draft
                .entry(id.to_string())
                .or_default()
                .values
                .insert(d.key.clone(), v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, enabled: bool, settings: Vec<SettingDef>) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: id.to_uppercase(),
            version: "1".into(),
            description: "d".into(),
            error: (id == "bad").then(|| "broken".into()),
            enabled,
            settings,
        }
    }

    fn every_kind() -> Vec<SettingDef> {
        let def = |key: &str, kind| SettingDef {
            key: key.into(),
            label: key.into(),
            kind,
            default: serde_json::Value::Null,
            help: "help".into(),
        };
        vec![
            def("s", SettingKind::String),
            def("b", SettingKind::Bool),
            def("n", SettingKind::Number { min: Some(0.0), max: None }),
            def("c", SettingKind::Choice(vec!["a".into()])),
            def("f", SettingKind::Folder),
        ]
    }

    fn draw(p: &mut PluginsUi) {
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                p.show(ctx);
            });
        }
    }

    /// Every tab drawn headless. Tests are debug builds, so egui's layout assertions (e.g.
    /// add_space in a grid) run.
    #[test]
    fn draws_every_tab() {
        let infos = vec![
            info("a", true, every_kind()),
            info("off", false, every_kind()),
            info("bad", true, vec![]),
        ];
        let mut p = PluginsUi::new(infos, &IndexMap::new(), "/".into(), "/p".into());
        draw(&mut p);
        // tabs: only loaded plugins with settings
        assert!(p.has_tab("a"));
        assert!(!p.has_tab("off") && !p.has_tab("bad"));
        p.tab = Tab::Plugin("a".into());
        draw(&mut p);
        // empty: the "no plugins" text
        draw(&mut PluginsUi::new(vec![], &IndexMap::new(), "/".into(), "/p".into()));
    }

    #[test]
    fn toggling_and_applying() {
        let infos = vec![info("a", true, every_kind())];
        let mut p = PluginsUi::new(infos.clone(), &IndexMap::new(), "/".into(), "/p".into());
        assert!(!p.dirty());
        // an explicit "enabled" entry is the same as none
        p.draft.insert("a".into(), PluginCfg::default());
        assert!(!p.dirty());
        p.draft.get_mut("a").unwrap().enabled = false;
        assert!(p.dirty() && !p.enabled("a"));
        // after applying, the dialog shows what's loaded and nothing is pending
        let saved = p.draft.clone();
        p.tab = Tab::Plugin("a".into());
        p.applied(vec![info("a", false, every_kind())], &saved);
        assert!(!p.dirty());
        assert!(p.tab == Tab::Manager); // its tab is gone
    }
}
