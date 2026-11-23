// SPDX-License-Identifier: Apache-2.0
//! `Host` for builds without the `plugins` feature: same API, loads nothing.

use std::path::{Path, PathBuf};

use eframe::egui;

use super::*;
use crate::render::{Extra, Job};
use crate::settings::Settings;

pub struct Host {
    dir: PathBuf,
}

#[allow(dead_code)]
impl Host {
    pub fn none() -> Self {
        Self {
            dir: PathBuf::new(),
        }
    }
    pub fn load(_: &Settings) -> Self {
        Self::none()
    }
    pub fn load_from(dir: PathBuf, _: &Settings) -> Self {
        Self { dir }
    }
    pub fn set_gui(&mut self, _: bool) {}
    pub fn reload(&mut self, _: &Settings) {}
    pub fn apply_settings(&mut self, _: &Settings) {}
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn is_active(&self) -> bool {
        false
    }
    pub fn infos(&self) -> Vec<PluginInfo> {
        Vec::new()
    }
    pub fn shortcut_items(&self, _: Screen) -> Vec<MenuItem> {
        Vec::new()
    }
    pub fn menus(&self, _: Screen) -> Vec<PluginMenu> {
        Vec::new()
    }
    pub fn toolbar_items(&self) -> Vec<ToolItem> {
        Vec::new()
    }
    pub fn clip_actions(&self) -> Vec<ClipAction> {
        Vec::new()
    }
    pub fn run_menu(&mut self, _: usize, _: usize, _: &Ctx) {}
    pub fn run_toolbar(&mut self, _: usize, _: usize, _: &Ctx) {}
    pub fn run_clip_action(
        &mut self,
        _: &str,
        _: &str,
        _: &[PathBuf],
        _: &Ctx,
    ) -> Result<(), String> {
        Err("this clipman was built without plugin support".into())
    }
    pub fn clip_opened(&mut self, _: &Ctx) {}
    pub fn export_start(&mut self, _: &Job) -> Result<Extra, String> {
        Ok(Extra::default())
    }
    pub fn export_done(&mut self, _: &Job, _: &Result<(PathBuf, u64), String>) {}
    pub fn browser_refresh(&mut self, _: &[PathBuf]) {}
    pub fn badges(&mut self, _: &Path) -> Vec<String> {
        Vec::new()
    }
    pub fn has_badges(&self) -> bool {
        false
    }
    pub fn draw_windows(&mut self, _: &egui::Context, _: &Ctx) {}
    pub fn has_open_windows(&self) -> bool {
        false
    }
    pub fn busy(&self) -> bool {
        false
    }
    pub fn poll(&mut self) -> bool {
        false
    }
    pub fn wait_idle(&mut self) {}
    pub fn take_clip_cmds(&mut self) -> Vec<ClipCmd> {
        Vec::new()
    }
    pub fn has_clip_cmds(&self) -> bool {
        false
    }
    pub fn take_notices(&mut self) -> Vec<String> {
        Vec::new()
    }
}
