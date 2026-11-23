// SPDX-License-Identifier: Apache-2.0
//! Clip metadata via ffprobe, and making sure ffmpeg/ffprobe are available.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

/// A `Command` for a helper binary that never flashes a console window on Windows.
pub fn quiet_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Where clipman downloads ffmpeg when it isn't installed. A per-user data dir, because
/// the executable's own folder may be read-only (AppImage mount, Program Files).
pub fn ffmpeg_download_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("clipman")
        .join("ffmpeg")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A helper binary (ffmpeg / ffprobe): next to our executable (portable installs), then on
/// PATH, then in the download dir. Falls back to the bare name.
fn tool(name: &str) -> PathBuf {
    let file = exe_name(name);
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(&file)))
        .filter(|p| p.is_file());
    beside_exe
        .or_else(|| find_in_path(&file))
        .or_else(|| Some(ffmpeg_download_dir().join(&file)).filter(|p| p.is_file()))
        .unwrap_or_else(|| PathBuf::from(file))
}

pub fn ffmpeg_bin() -> PathBuf {
    tool("ffmpeg")
}

pub fn ffprobe_bin() -> PathBuf {
    tool("ffprobe")
}

fn runs(bin: &Path) -> bool {
    quiet_command(bin)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Make sure ffmpeg and ffprobe can be run, downloading a build into
/// `ffmpeg_download_dir()` when they're missing.
pub fn ensure_ffmpeg() -> Result<()> {
    if runs(&ffmpeg_bin()) && runs(&ffprobe_bin()) {
        return Ok(());
    }
    #[cfg(feature = "download")]
    {
        use ffmpeg_sidecar::download::{
            download_ffmpeg_package, ffmpeg_download_url, unpack_ffmpeg,
        };
        let dir = ffmpeg_download_dir();
        eprintln!("ffmpeg not found, downloading it to {}…", dir.display());
        std::fs::create_dir_all(&dir)?;
        let url = ffmpeg_download_url().context("no ffmpeg download for this platform")?;
        let archive = download_ffmpeg_package(url, &dir).context("couldn't download ffmpeg")?;
        unpack_ffmpeg(&archive, &dir).context("couldn't unpack ffmpeg")?;
        if runs(&ffmpeg_bin()) && runs(&ffprobe_bin()) {
            return Ok(());
        }
        anyhow::bail!("downloaded ffmpeg doesn't run")
    }
    #[cfg(not(feature = "download"))]
    anyhow::bail!("ffmpeg / ffprobe not found")
}

#[derive(Clone, Debug, Default)]
pub struct ClipInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// One entry per audio stream: its name tag (set in GPU Screen Recorder), if any.
    pub audio_names: Vec<Option<String>>,
}

impl ClipInfo {
    pub fn aspect(&self) -> f32 {
        if self.width > 0 && self.height > 0 {
            self.width as f32 / self.height as f32
        } else {
            16.0 / 9.0
        }
    }
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

pub fn probe(path: &Path) -> Result<ClipInfo> {
    let out = quiet_command(ffprobe_bin())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type,width,height,r_frame_rate:stream_tags=name,title",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .context("couldn't run ffprobe")?;
    if !out.status.success() {
        anyhow::bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let p: Probe = serde_json::from_slice(&out.stdout).context("unexpected ffprobe output")?;
    let mut info = ClipInfo {
        duration: p
            .format
            .and_then(|f| f.duration)
            .and_then(|d| d.parse().ok())
            .unwrap_or(0.0),
        ..Default::default()
    };
    for s in p.streams {
        match s.codec_type.as_deref() {
            Some("video") if info.width == 0 => {
                info.width = s.width.unwrap_or(0);
                info.height = s.height.unwrap_or(0);
                info.fps = s.r_frame_rate.as_deref().map(parse_rate).unwrap_or(0.0);
            }
            Some("audio") => {
                let name = s.tags.get("name").or_else(|| s.tags.get("title")).cloned();
                info.audio_names.push(name.filter(|n| !n.trim().is_empty()));
            }
            _ => {}
        }
    }
    Ok(info)
}

/// One container-level tag (e.g. the `clipman` JSON of an exported clip).
#[cfg_attr(not(feature = "plugins"), allow(dead_code))]
pub fn container_tag(path: &Path, key: &str) -> Option<String> {
    let out = quiet_command(ffprobe_bin())
        .args(["-v", "error", "-show_entries"])
        .arg(format!("format_tags={key}"))
        .args(["-of", "json"])
        .arg(path)
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let tags = v.get("format")?.get("tags")?.as_object()?;
    // container tag keys can come back in another case
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
        .map(str::to_string)
}

fn parse_rate(r: &str) -> f64 {
    match r.split_once('/') {
        Some((a, b)) => {
            let (a, b): (f64, f64) = (a.parse().unwrap_or(0.0), b.parse().unwrap_or(1.0));
            if b > 0.0 { a / b } else { 0.0 }
        }
        None => r.parse().unwrap_or(0.0),
    }
}

/// Keyframe timestamps in seconds, from packet flags (fast, no decoding).
/// Timestamps (seconds) of every video frame and of the keyframes, from packet info
/// (fast, no decoding). Real timestamps, so frame stepping also works for variable
/// frame rate clips.
#[derive(Clone, Debug, Default)]
pub struct FrameTimes {
    pub all: Vec<f64>,
    pub keys: Vec<f64>,
}

pub fn frame_times(path: &Path) -> FrameTimes {
    let out = quiet_command(ffprobe_bin())
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,flags",
        ])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output();
    let mut ft = FrameTimes::default();
    for l in out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
    {
        let Some((t, flags)) = l.split_once(',') else {
            continue;
        };
        let Ok(t) = t.parse::<f64>() else { continue };
        ft.all.push(t);
        if flags.contains('K') {
            ft.keys.push(t);
        }
    }
    ft.all.sort_by(f64::total_cmp); // packets are in decode order
    ft.keys.sort_by(f64::total_cmp);
    if ft.keys.is_empty() {
        ft.keys.push(0.0);
    }
    ft
}

/// Index of the frame showing at time `t` (the last one starting at or before it).
pub fn frame_index(all: &[f64], t: f64) -> usize {
    all.partition_point(|&p| p <= t + 0.0005).saturating_sub(1)
}

/// The last keyframe at or before `t` (a lossless cut has to start on one).
pub fn keyframe_before(keyframes: &[f64], t: f64) -> f64 {
    let i = keyframes.partition_point(|&k| k <= t + 0.001);
    keyframes[i.saturating_sub(1)]
}

/// "HH:MM:SS.xx" (ffmpeg progress) to seconds.
pub fn parse_ffmpeg_time(t: &str) -> Option<f64> {
    let mut secs = 0.0;
    for part in t.trim().trim_start_matches('-').split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(secs)
}

/// "1:05.25" for the UI.
pub fn fmt_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    format!("{}:{:05.2}", (secs / 60.0) as u64, secs % 60.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_ffmpeg_time("00:01:02.50"), Some(62.5));
        assert_eq!(fmt_time(65.25), "1:05.25");
        assert_eq!(parse_rate("60/1"), 60.0);
        let kf = [0.0, 2.0, 4.0];
        assert_eq!(keyframe_before(&kf, 3.9), 2.0);
        assert_eq!(keyframe_before(&kf, 4.0), 4.0);
    }
}
