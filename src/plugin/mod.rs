// SPDX-License-Identifier: Apache-2.0
//! Lua plugins: `<config>/plugins/<id>/init.lua` returns a table of menu items, toolbar
//! buttons, windows, settings, browser additions and event hooks (see docs/plugins.md).
//!
//! Lua only ever runs on the UI thread (or the main thread for command-line renders):
//! everything a plugin adds to a render is collected into `render::Extra` before the job
//! starts. Plugins are trusted like a config file: they get the whole Lua standard library.
//!
//! Built without the `plugins` feature, `Host` is a stub with the same API that loads
//! nothing.

// without the feature, most of this is only used by tests
#![cfg_attr(not(feature = "plugins"), allow(dead_code))]

pub mod meta;

#[cfg(feature = "plugins")]
mod host;
#[cfg(not(feature = "plugins"))]
#[path = "stub.rs"]
mod host;
#[cfg(feature = "plugins")]
mod ui;

pub use host::Host;

use std::path::PathBuf;

/// Version of the plugin API. A plugin's `api` field must not be newer.
pub const API_VERSION: i64 = 1;

/// The clip open in the editor, as plugins see it.
#[derive(Clone, Debug, Default)]
pub struct ClipState {
    pub path: PathBuf,
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub audio_names: Vec<Option<String>>,
    pub start: f64,
    pub end: f64,
    pub playhead: f64,
}

/// What a plugin callback runs in: which screen is showing, and the editor's clip.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub screen: Screen,
    pub clip: Option<ClipState>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Screen {
    #[default]
    Browser,
    Editor,
    /// `--plugin-action` or a command-line render: no browser or editor.
    Other,
}

impl Screen {
    pub fn name(self) -> &'static str {
        match self {
            Screen::Browser => "browser",
            Screen::Editor => "editor",
            Screen::Other => "other",
        }
    }
}

/// A timeline marker added by a plugin.
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub plugin: String,
    pub t: f64,
    pub label: String,
    pub color: Option<[u8; 3]>,
}

/// Changes plugins asked for on the editor's clip, applied by the editor.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipCmd {
    AddMarker(Marker),
    /// Remove this plugin's markers.
    ClearMarkers(String),
    SetRange(f64, f64),
    Seek(f64),
}

/// An item in one of a plugin's dropdown menus.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub plugin: usize,
    pub item: usize,
    pub label: String,
    pub shortcut: Option<eframe::egui::KeyboardShortcut>,
}

/// A dropdown in the menu bar that plugins made. Plugins using the same title share one
/// dropdown (a separator between them).
#[derive(Clone, Debug)]
pub struct PluginMenu {
    pub title: String,
    pub entries: Vec<MenuEntry>,
}

#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
    Submenu(String, Vec<MenuEntry>),
}

/// Menu bar titles clipman uses itself.
pub const RESERVED_MENUS: [&str; 3] = ["File", "Plugins", "Help"];

/// A button a plugin adds to the editor's control row.
#[derive(Clone, Debug)]
pub struct ToolItem {
    pub plugin: usize,
    pub item: usize,
    pub label: String,
    pub tooltip: String,
}

/// A per-clip action (browser right-click menu, and the file manager's when
/// `file_manager` is set).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipAction {
    pub plugin_id: String,
    pub id: String,
    pub label: String,
    pub file_manager: bool,
}

impl ClipAction {
    /// The `--plugin-action` argument.
    pub fn arg(&self) -> String {
        format!("{}:{}", self.plugin_id, self.id)
    }
}

/// A setting a plugin declares; the Settings dialog draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingDef {
    pub key: String,
    pub label: String,
    pub kind: SettingKind,
    pub default: serde_json::Value,
    pub help: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind {
    String,
    Bool,
    Number { min: Option<f64>, max: Option<f64> },
    Choice(Vec<String>),
    Folder,
}

/// What the Settings dialog shows about a plugin.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// Didn't load, or failed since.
    pub error: Option<String>,
    pub enabled: bool,
    pub settings: Vec<SettingDef>,
}

/// Escape text for use as a filter option value inside a `-vf` / `-af` chain (e.g.
/// drawtext's `text=`): first for the option parser (`\ ' :`), then for the filter
/// graph (`\ ' [ ] , ;`). No quotes needed around the result.
pub fn filter_escape(s: &str) -> String {
    let esc = |s: &str, special: &[char]| {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if special.contains(&c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let value = esc(s, &['\\', '\'', ':']);
    esc(&value, &['\\', '\'', '[', ']', ',', ';'])
}

/// Plugin ids are folder names; they end up in Thunar ids and command lines.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parse "Ctrl+Shift+G" style shortcuts (Ctrl is Cmd on macOS).
pub fn parse_shortcut(s: &str) -> Option<eframe::egui::KeyboardShortcut> {
    use eframe::egui::{Key, KeyboardShortcut, Modifiers};
    let mut mods = Modifiers::NONE;
    let mut key = None;
    for part in s.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "cmd" | "command" => mods = mods.plus(Modifiers::COMMAND),
            "shift" => mods = mods.plus(Modifiers::SHIFT),
            "alt" | "option" => mods = mods.plus(Modifiers::ALT),
            _ if key.is_none() => {
                key = Some(Key::from_name(part).or_else(|| Key::from_name(&part.to_uppercase()))?)
            }
            _ => return None,
        }
    }
    Some(KeyboardShortcut::new(mods, key?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_filter_values() {
        // checked against ffmpeg's drawtext
        assert_eq!(filter_escape("it's 50%: ok"), r"it\\\'s 50%\\: ok");
        assert_eq!(filter_escape(r"a\b [x], y; z"), r"a\\\\b \[x\]\, y\; z");
        assert_eq!(filter_escape(r"C:\path"), r"C\\:\\\\path");
    }

    #[test]
    fn ids_and_shortcuts() {
        assert!(valid_id("game-tags_2"));
        assert!(!valid_id("Game"));
        assert!(!valid_id("a b"));
        assert!(!valid_id(""));
        use eframe::egui::{Key, Modifiers};
        let sc = parse_shortcut("Ctrl+Shift+g").unwrap();
        assert_eq!(sc.logical_key, Key::G);
        assert_eq!(sc.modifiers, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(parse_shortcut("F5").unwrap().logical_key, Key::F5);
        assert!(parse_shortcut("Ctrl+").is_none());
        assert!(parse_shortcut("Ctrl+A+B").is_none());
    }
}
