// SPDX-License-Identifier: Apache-2.0
//! Clip metadata via ffprobe, and making sure ffmpeg/ffprobe are available.

use std::path::Path;
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

/// Make sure ffmpeg (and ffprobe) can be run. When they're missing, download a build
/// next to our executable (ffmpeg-sidecar does this for Windows, macOS and Linux).
pub fn ensure_ffmpeg() -> Result<()> {
    if ffmpeg_sidecar::command::ffmpeg_is_installed()
        && ffmpeg_sidecar::ffprobe::ffprobe_is_installed()
    {
        return Ok(());
    }
    #[cfg(feature = "download")]
    {
        eprintln!("ffmpeg not found, downloading it…");
        ffmpeg_sidecar::download::auto_download().context("couldn't download ffmpeg")?;
        Ok(())
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
    let out = quiet_command(ffmpeg_sidecar::ffprobe::ffprobe_path())
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
pub fn keyframes(path: &Path) -> Vec<f64> {
    let out = quiet_command(ffmpeg_sidecar::ffprobe::ffprobe_path())
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
    let mut kf: Vec<f64> = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (t, flags) = l.split_once(',')?;
            flags.contains('K').then(|| t.parse().ok()).flatten()
        })
        .collect();
    kf.sort_by(f64::total_cmp);
    if kf.is_empty() {
        kf.push(0.0);
    }
    kf
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
