// SPDX-License-Identifier: Apache-2.0
//! Settings shared by every mode: presets, audio track rules, noise suppression,
//! output naming and the clip browser folder. Stored as JSON in the platform config
//! dir (`~/.config/clipman/settings.json` on Linux, `%APPDATA%\clipman\` on Windows).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_MB: f64 = 20.0; // Discord's upload limit

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresetKind {
    /// Re-encode: fit under `max_mb` (or a fixed quality when there is no limit).
    Compress,
    /// Just trim: copy the video stream untouched.
    Original,
}

fn kind_compress() -> PresetKind {
    PresetKind::Compress
}
fn d_height() -> u32 {
    720
}
fn d_fps() -> u32 {
    60
}
fn d_max_mb() -> Option<f64> {
    Some(DEFAULT_MAX_MB)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    /// Stable id; Thunar entries refer to it, so it never changes after creation.
    pub id: String,
    #[serde(rename = "type", default = "kind_compress")]
    pub kind: PresetKind,
    /// Blank = shown as e.g. "720p60".
    #[serde(default)]
    pub name: String,
    #[serde(default = "d_height")]
    pub height: u32,
    #[serde(default = "d_fps")]
    pub fps: u32,
    /// None = no size limit (fixed quality).
    #[serde(default = "d_max_mb")]
    pub max_mb: Option<f64>,
    #[serde(default)]
    pub suffix: String,
}

impl Preset {
    pub fn is_original(&self) -> bool {
        self.kind == PresetKind::Original
    }

    pub fn resolution(&self) -> String {
        format!("{}p{}", self.height, self.fps)
    }

    /// Short name: the preset's name, or e.g. "720p60" when it's blank.
    pub fn label(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            name.to_string()
        } else if self.is_original() {
            "Original quality".into()
        } else {
            self.resolution()
        }
    }

    pub fn size_text(&self) -> String {
        match self.max_mb {
            Some(mb) if mb > 0.0 => format!("{} MB", fmt_num(mb)),
            _ => "no size limit".into(),
        }
    }

    /// Text for dropdowns / menus, e.g. "720p60 · 20 MB" or "Sharp (1080p30 · no size limit)".
    pub fn menu_text(&self) -> String {
        if self.is_original() {
            return format!("{} (just trim, no compression)", self.label());
        }
        let (label, res) = (self.label(), self.resolution());
        if label == res {
            format!("{res} · {}", self.size_text())
        } else {
            format!("{label} ({res} · {})", self.size_text())
        }
    }

    /// Size limit in MB, treating 0 / negative as "no limit".
    pub fn limit_mb(&self) -> Option<f64> {
        self.max_mb.filter(|mb| *mb > 0.0)
    }
}

/// "20" instead of "20.0", "8.5" stays "8.5".
pub fn fmt_num(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Denoise {
    /// No noise suppression option for this track (e.g. game audio).
    None,
    /// Offered in the Audio dropdown, unticked.
    #[default]
    Off,
    /// Offered and ticked by default.
    On,
}

fn d_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrackCfg {
    #[serde(default)]
    pub label: String,
    #[serde(default = "d_true")]
    pub default: bool,
    #[serde(default)]
    pub denoise: Denoise,
}

impl Default for TrackCfg {
    fn default() -> Self {
        Self {
            label: String::new(),
            default: true,
            denoise: Denoise::Off,
        }
    }
}

fn d_strength() -> u8 {
    100
}
fn d_prefix() -> String {
    "remuxed_".into()
}
fn d_presets() -> Vec<Preset> {
    default_presets()
}
fn d_tracks() -> IndexMap<String, TrackCfg> {
    let t = |label: &str, denoise| TrackCfg {
        label: label.into(),
        default: true,
        denoise,
    };
    IndexMap::from([
        ("Game".to_string(), t("Game", Denoise::None)),
        ("Voice".to_string(), t("Voice", Denoise::Off)),
        ("Discord".to_string(), t("Discord", Denoise::Off)),
    ])
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Noise suppression strength in percent; becomes arnndn's `mix`.
    #[serde(default = "d_strength")]
    pub denoise_strength: u8,
    /// Every output is `<prefix><clip name><preset suffix>.mp4`.
    #[serde(default = "d_prefix")]
    pub output_prefix: String,
    /// Folder shown by the clip browser.
    #[serde(default)]
    pub clips_folder: Option<PathBuf>,
    #[serde(default = "d_presets")]
    pub presets: Vec<Preset>,
    /// Keyed by the track's name tag in the clip (as set in GPU Screen Recorder).
    #[serde(default = "d_tracks")]
    pub tracks: IndexMap<String, TrackCfg>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            denoise_strength: d_strength(),
            output_prefix: d_prefix(),
            clips_folder: None,
            presets: d_presets(),
            tracks: d_tracks(),
        }
    }
}

pub fn default_presets() -> Vec<Preset> {
    let compress = |id: &str, height, fps, suffix: &str| Preset {
        id: id.into(),
        kind: PresetKind::Compress,
        name: String::new(),
        height,
        fps,
        max_mb: Some(DEFAULT_MAX_MB),
        suffix: suffix.into(),
    };
    vec![
        compress("primary", 720, 60, ""),
        compress("secondary", 1080, 30, "_1080p"),
        Preset {
            id: "original".into(),
            kind: PresetKind::Original,
            name: "Original quality".into(),
            height: 0,
            fps: 0,
            max_mb: None,
            suffix: String::new(),
        },
    ]
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("clipman")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("clipman")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let mut s = std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
            .unwrap_or_default();
        s.denoise_strength = s.denoise_strength.min(100);
        s.presets.retain(|p| !p.id.is_empty());
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = settings_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap() + "\n")?;
        std::fs::rename(tmp, path)
    }

    pub fn clips_folder(&self) -> PathBuf {
        self.clips_folder
            .clone()
            .or_else(dirs::video_dir)
            .or_else(|| {
                dirs::home_dir()
                    .map(|h| h.join("Videos"))
                    .filter(|v| v.is_dir())
            })
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn preset(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    pub fn first_compress_preset(&self) -> Option<&Preset> {
        self.presets.iter().find(|p| !p.is_original())
    }

    /// Settings for one audio track (label, ticked by default, noise suppression).
    pub fn track(&self, name: Option<&str>, idx: usize) -> TrackCfg {
        let mut t = name
            .and_then(|n| self.tracks.get(n))
            .cloned()
            .unwrap_or_default();
        if t.label.trim().is_empty() {
            t.label = name
                .map(str::to_string)
                .unwrap_or_else(|| format!("Track {}", idx + 1));
        }
        t
    }

    /// (tracks, denoise) a clip gets when nothing is chosen explicitly.
    pub fn default_audio(&self, names: &[Option<String>]) -> (Vec<usize>, Vec<usize>) {
        let infos: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, n)| self.track(n.as_deref(), i))
            .collect();
        let tracks: Vec<usize> = (0..infos.len()).filter(|&i| infos[i].default).collect();
        let denoise = tracks
            .iter()
            .copied()
            .filter(|&i| infos[i].denoise == Denoise::On)
            .collect();
        (tracks, denoise)
    }

    /// `<prefix><clip name><suffix>.mp4` next to `src`, numbered (…2, …3) instead of
    /// overwriting anything, including the source when prefix and suffix are empty.
    pub fn output_path(&self, src: &Path, suffix: &str) -> PathBuf {
        let dir = src.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut stem = src
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let prefix = self.output_prefix.replace(['/', '\\'], "_");
        if !prefix.is_empty() && stem.starts_with(&prefix) {
            stem = stem[prefix.len()..].to_string(); // don't stack prefixes
        }
        let base = format!("{prefix}{stem}{suffix}");
        let mut out = dir.join(format!("{base}.mp4"));
        let mut n = 2;
        while out.exists() {
            out = dir.join(format!("{base}{n}.mp4"));
            n += 1;
        }
        out
    }

    /// A new preset id that isn't used yet.
    pub fn new_preset_id(&self) -> String {
        (1..)
            .map(|n| format!("custom-{n}"))
            .find(|id| self.preset(id).is_none())
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let p = &default_presets();
        assert_eq!(p[0].menu_text(), "720p60 · 20 MB");
        assert_eq!(
            p[2].menu_text(),
            "Original quality (just trim, no compression)"
        );
        let mut named = p[1].clone();
        named.name = "Sharp".into();
        named.max_mb = None;
        assert_eq!(named.menu_text(), "Sharp (1080p30 · no size limit)");
    }

    #[test]
    fn parses_partial_json() {
        let old = r#"{"denoise_strength": 60, "output_prefix": "x_", "presets": [
            {"id":"primary","type":"compress","name":"","height":720,"fps":60,"max_mb":8.5,"suffix":""},
            {"id":"custom-1","type":"compress","name":"","height":1080,"fps":60,"max_mb":null,"suffix":"_hq"},
            {"id":"original","type":"original","name":"Original quality","suffix":""}],
            "tracks": {"Voice": {"label":"Mic","default":true,"denoise":"on"}}}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.denoise_strength, 60);
        assert_eq!(s.presets[0].max_mb, Some(8.5));
        assert_eq!(s.presets[1].max_mb, None);
        assert!(s.presets[2].is_original());
        assert_eq!(s.tracks["Voice"].denoise, Denoise::On);
        assert_eq!(s.clips_folder, None);
    }

    #[test]
    fn track_fallbacks() {
        let s = Settings::default();
        assert_eq!(s.track(None, 1).label, "Track 2");
        assert_eq!(
            s.track(Some("Devices: Default input"), 0).label,
            "Devices: Default input"
        );
        let names = vec![Some("Game".into()), Some("Voice".into()), None];
        assert_eq!(s.default_audio(&names), (vec![0, 1, 2], vec![]));
    }

    #[test]
    fn output_names() {
        let dir = std::env::temp_dir().join(format!("clipman-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("clip.mp4");
        std::fs::write(&src, b"x").unwrap();
        let mut s = Settings::default();
        assert_eq!(
            s.output_path(&src, "").file_name().unwrap(),
            "remuxed_clip.mp4"
        );
        assert_eq!(
            s.output_path(&dir.join("remuxed_clip.mp4"), "_1080p")
                .file_name()
                .unwrap(),
            "remuxed_clip_1080p.mp4"
        );
        s.output_prefix.clear();
        assert_eq!(s.output_path(&src, "").file_name().unwrap(), "clip2.mp4"); // never the source
        std::fs::remove_dir_all(dir).unwrap();
    }
}
