// SPDX-License-Identifier: Apache-2.0
//! Rendering: size-targeted 2-pass x264, fixed-quality (CRF) encodes, lossless trims,
//! audio track mixing and RNNoise noise suppression. Used by the editor (progress shown
//! in its window) and by the command line / right-click menu.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};

use crate::media::{self, parse_ffmpeg_time};
use crate::settings::{Preset, cache_dir};

const TARGET_FRACTION: f64 = 0.97; // aim a bit under the limit; retry if over
const MAX_ATTEMPTS: u32 = 3;
const AUDIO_KBPS: u32 = 96;
const MAX_VIDEO_KBPS: u32 = 16_000; // very short trims don't need more
const NO_LIMIT_CRF: u32 = 20;
const NO_LIMIT_AUDIO_KBPS: u32 = 160;
const ORIGINAL_MIX_AUDIO_KBPS: u32 = 192;

// RNNoise model "leavened-quisling" (voice vs. general noise), fetched on first use
// and pinned to a commit + checksum. Not shipped with the app.
const RNNOISE_URL: &str = "https://raw.githubusercontent.com/GregorR/rnnoise-models/\
3eee541a283fd3b8f81b85b1748e3b9ccbefa04d/leavened-quisling-2018-08-31/lq.rnnn";
const RNNOISE_SHA256: &str = "1957528b752799fddf06270bc5469af7cf54c3badc358544ae2abed730943ff9";
const RNNOISE_FILE: &str = "lq.rnnn";

/// What to render.
#[derive(Clone, Debug)]
pub struct Job {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub preset: Preset,
    /// Trim range in seconds (None = from the start / to the end).
    pub start: Option<f64>,
    pub end: Option<f64>,
    /// 0-based audio streams to mix into one track.
    pub tracks: Vec<usize>,
    /// Subset of `tracks` to run RNNoise on.
    pub denoise: Vec<usize>,
    /// Noise suppression strength in percent.
    pub strength: u8,
}

/// Progress reports: fraction 0..1 of the whole job and a short description.
pub type Progress<'a> = &'a mut dyn FnMut(f32, &str);

pub struct Cancelled;

impl std::fmt::Debug for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}
impl std::error::Error for Cancelled {}

// ---------------------------------------------------------------------------
// RNNoise model
// ---------------------------------------------------------------------------

pub fn model_path() -> PathBuf {
    cache_dir().join(RNNOISE_FILE)
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn model_ready() -> bool {
    std::fs::read(model_path())
        .map(|d| sha256_hex(&d) == RNNOISE_SHA256)
        .unwrap_or(false)
}

/// Path to the verified RNNoise model, downloading it if needed. Safe to call from
/// several threads/processes at once (unique temp file + atomic rename).
pub fn ensure_model() -> Result<PathBuf> {
    let path = model_path();
    if model_ready() {
        return Ok(path);
    }
    #[cfg(not(feature = "download"))]
    bail!(
        "built without download support; put {RNNOISE_URL} at {}",
        path.display()
    );
    #[cfg(feature = "download")]
    {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let body = ureq::get(RNNOISE_URL)
            .call()
            .and_then(|mut r| r.body_mut().read_to_vec())
            .context("couldn't download the noise suppression model")?;
        if sha256_hex(&body) != RNNOISE_SHA256 {
            bail!("the downloaded noise suppression model failed its checksum");
        }
        let tmp = path.with_extension(format!("part{}-{}", std::process::id(), unique()));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(path)
    }
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// How the selected audio tracks end up in the output.
#[derive(Debug, PartialEq)]
pub enum AudioPlan {
    None,
    /// A single track used as-is (can be stream-copied).
    Direct(usize),
    /// A filter graph whose output pad is `[aout]`. Uses the model by its file name,
    /// so ffmpeg must run with the model's directory as its working directory (this
    /// sidesteps filter-graph escaping of Windows paths like `C:\…`).
    Graph(String),
}

pub fn audio_plan(tracks: &[usize], denoise: &[usize], strength: u8) -> AudioPlan {
    let denoise: Vec<usize> = if strength == 0 {
        vec![]
    } else {
        tracks
            .iter()
            .copied()
            .filter(|t| denoise.contains(t))
            .collect()
    };
    match (tracks, denoise.is_empty()) {
        ([], _) => AudioPlan::None,
        ([t], true) => AudioPlan::Direct(*t),
        _ => {
            let mix = format!("{:.2}", strength as f64 / 100.0);
            let mut chains = Vec::new();
            let mut ins = String::new();
            for &t in tracks {
                if denoise.contains(&t) {
                    // arnndn occasionally emits NaN / huge samples (mostly in its end-of-stream
                    // flush); zero them so encoders don't reject the audio. Don't clamp
                    // instead: that turns the spikes into full-scale clicks.
                    chains.push(format!(
                        "[0:a:{t}]arnndn=m={RNNOISE_FILE}:mix={mix},\
                         aeval=exprs='if(isnan(val(ch))+gt(abs(val(ch)),2),0,val(ch))':c=same[dn{t}]"
                    ));
                    ins += &format!("[dn{t}]");
                } else {
                    ins += &format!("[0:a:{t}]");
                }
            }
            if tracks.len() == 1 {
                let only = chains.pop().unwrap();
                let cut = only.rfind('[').unwrap();
                return AudioPlan::Graph(format!("{}[aout]", &only[..cut]));
            }
            // normalize=0: sum tracks like a recorder's own pre-mixed track does
            chains.push(format!(
                "{ins}amix=inputs={}:normalize=0:duration=longest[aout]",
                tracks.len()
            ));
            AudioPlan::Graph(chains.join(";"))
        }
    }
}

impl AudioPlan {
    pub fn needs_model(&self) -> bool {
        matches!(self, AudioPlan::Graph(g) if g.contains("arnndn"))
    }

    /// Add the mapping / filter args (not the codec) to an ffmpeg command.
    pub fn apply(&self, cmd: &mut FfmpegCommand) {
        match self {
            AudioPlan::None => {
                cmd.no_audio();
            }
            AudioPlan::Direct(t) => {
                cmd.map(format!("0:a:{t}"));
            }
            AudioPlan::Graph(g) => {
                cmd.filter_complex(g).map("[aout]");
            }
        }
    }
}

/// Run every ffmpeg from the cache dir so `arnndn=m=lq.rnnn` resolves.
pub fn ffmpeg() -> FfmpegCommand {
    let mut cmd = FfmpegCommand::new();
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    cmd.as_inner_mut().current_dir(dir);
    cmd.hide_banner();
    cmd
}

// ---------------------------------------------------------------------------
// Running ffmpeg with progress
// ---------------------------------------------------------------------------

/// Run a prepared command to completion, reporting progress over `[base, base+span]`
/// of the job relative to `duration` seconds of output.
fn run(
    cmd: &mut FfmpegCommand,
    duration: f64,
    base: f32,
    span: f32,
    label: &str,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<()> {
    progress(base, label);
    if std::env::var_os("CLIPMAN_DEBUG").is_some() {
        eprintln!();
        cmd.print_command();
    }
    let mut child = cmd.spawn().context("couldn't start ffmpeg")?;
    let mut errors = Vec::new();
    for event in child.iter()? {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Cancelled.into());
        }
        match event {
            FfmpegEvent::Progress(p) => {
                if let Some(t) = parse_ffmpeg_time(&p.time) {
                    let f = if duration > 0.0 {
                        (t / duration).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    progress(base + span * f as f32, label);
                }
            }
            FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) => errors.push(msg),
            FfmpegEvent::Error(msg) => errors.push(msg),
            _ => {}
        }
    }
    let status = child.wait()?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    if !status.success() {
        let tail: Vec<_> = errors.iter().rev().take(6).rev().cloned().collect();
        bail!(
            "ffmpeg failed: {}",
            if tail.is_empty() {
                status.to_string()
            } else {
                tail.join("\n")
            }
        );
    }
    Ok(())
}

/// Temporary working directory, removed on drop.
struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Result<Self> {
        let d = std::env::temp_dir().join(format!("clipman-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&d)?;
        Ok(Self(d))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?; // across filesystems (e.g. /tmp is tmpfs)
        std::fs::remove_file(from)?;
    }
    Ok(())
}

fn null_output() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Render `job`. Returns the output path and its size in bytes.
pub fn render(job: &Job, progress: Progress, cancel: &AtomicBool) -> Result<(PathBuf, u64)> {
    // ffmpeg runs from the cache dir, so relative paths would resolve against that
    let job = &Job {
        src: std::path::absolute(&job.src)?,
        dst: std::path::absolute(&job.dst)?,
        ..job.clone()
    };
    let info = media::probe(&job.src)?;
    let start = job.start.unwrap_or(0.0).max(0.0);
    let end = job
        .end
        .map(|e| e.min(info.duration))
        .unwrap_or(info.duration);
    if end - start <= 0.1 {
        bail!("the selected range is empty");
    }
    let plan = audio_plan(&job.tracks, &job.denoise, job.strength);
    if plan.needs_model() {
        progress(0.0, "Getting noise suppression model…");
        ensure_model()?;
    }
    let tmp = TempDir::new()?;
    let out = tmp.0.join("out.mp4");
    if job.preset.is_original() {
        render_original(job, &info, start, end, &plan, &out, progress, cancel)?;
    } else {
        render_compress(
            job, &info, start, end, &plan, &tmp.0, &out, progress, cancel,
        )?;
    }
    move_file(&out, &job.dst)?;
    let size = std::fs::metadata(&job.dst)?.len();
    progress(1.0, "Done");
    Ok((job.dst.clone(), size))
}

fn trim(cmd: &mut FfmpegCommand, start: f64, dur: f64) {
    cmd.seek(format!("{start:.3}"))
        .duration(format!("{dur:.3}"));
}

#[allow(clippy::too_many_arguments)]
fn render_original(
    job: &Job,
    info: &media::ClipInfo,
    start: f64,
    end: f64,
    plan: &AudioPlan,
    out: &Path,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<()> {
    // Lossless video cut: start on the keyframe at/before `start` so the copy doesn't
    // begin with frozen frames.
    let k = media::keyframe_before(&media::keyframes(&job.src), start);
    let mut cmd = ffmpeg();
    cmd.seek(format!("{:.3}", k + 0.001))
        .input(&job.src)
        .duration(format!("{:.3}", end - k));
    cmd.map("0:v:0").codec_video("copy");
    plan.apply(&mut cmd);
    match plan {
        AudioPlan::None => {}
        AudioPlan::Direct(_) => {
            cmd.codec_audio("copy");
        }
        AudioPlan::Graph(_) => {
            cmd.codec_audio("aac")
                .args(["-b:a", &format!("{ORIGINAL_MIX_AUDIO_KBPS}k")]);
        }
    }
    cmd.args(["-avoid_negative_ts", "make_zero", "-movflags", "+faststart"])
        .overwrite()
        .output(out);
    let _ = info;
    run(
        &mut cmd,
        end - k,
        0.0,
        1.0,
        "Trimming (original quality)",
        progress,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_compress(
    job: &Job,
    info: &media::ClipInfo,
    start: f64,
    end: f64,
    plan: &AudioPlan,
    tmp: &Path,
    out: &Path,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<()> {
    let p = &job.preset;
    let duration = end - start;
    let trimmed = start > 0.0 || end < info.duration;
    let vf = format!("scale=-2:'min({},ih)':flags=lanczos", p.height); // never upscale
    let base = |cmd: &mut FfmpegCommand| {
        if trimmed {
            trim(cmd, start, duration);
        }
        cmd.input(&job.src).map("0:v:0").args(["-vf", vf.as_str()]);
        cmd.args(["-fpsmax", &p.fps.to_string()])
            .codec_video("libx264")
            .preset("slow")
            .pix_fmt("yuv420p");
    };

    let Some(limit_mb) = p.limit_mb() else {
        // no size limit: one pass at a fixed quality
        let mut cmd = ffmpeg();
        base(&mut cmd);
        cmd.crf(NO_LIMIT_CRF);
        plan.apply(&mut cmd);
        if *plan != AudioPlan::None {
            cmd.codec_audio("aac")
                .args(["-b:a", &format!("{NO_LIMIT_AUDIO_KBPS}k")]);
        }
        cmd.args(["-movflags", "+faststart"])
            .overwrite()
            .output(out);
        return run(
            &mut cmd,
            duration,
            0.0,
            1.0,
            "Encoding (no size limit)",
            progress,
            cancel,
        );
    };

    let limit = limit_mb * 1e6;
    let target0 = limit * TARGET_FRACTION;
    let mut target = target0;
    let audio_kbps = if *plan == AudioPlan::None {
        0
    } else {
        AUDIO_KBPS
    };
    let passlog = tmp.join("pass");
    for attempt in 1..=MAX_ATTEMPTS {
        let total_kbps = target * 8.0 / 1000.0 / duration;
        let v_kbps = ((total_kbps - audio_kbps as f64) as u32).min(MAX_VIDEO_KBPS);
        if v_kbps < 150 {
            bail!(
                "the clip is too long ({duration:.0}s) to fit under {} MB at watchable quality",
                crate::settings::fmt_num(limit_mb)
            );
        }
        let rate = |cmd: &mut FfmpegCommand| {
            cmd.args([
                "-b:v",
                &format!("{v_kbps}k"),
                "-maxrate",
                &format!("{}k", v_kbps * 3 / 2),
            ]);
            cmd.args(["-bufsize", &format!("{}k", v_kbps * 2)]);
            cmd.args(["-passlogfile"]).arg(&passlog);
        };
        let retry = if attempt == 1 {
            String::new()
        } else {
            format!(" (retry {})", attempt - 1)
        };

        let mut pass1 = ffmpeg();
        base(&mut pass1);
        rate(&mut pass1);
        pass1
            .args(["-pass", "1"])
            .no_audio()
            .format("null")
            .overwrite()
            .output(null_output());
        run(
            &mut pass1,
            duration,
            0.0,
            0.45,
            &format!("Pass 1/2: analyzing{retry}"),
            progress,
            cancel,
        )?;

        let mut pass2 = ffmpeg();
        base(&mut pass2);
        rate(&mut pass2);
        pass2.args(["-pass", "2"]);
        plan.apply(&mut pass2);
        if audio_kbps > 0 {
            pass2
                .codec_audio("aac")
                .args(["-b:a", &format!("{audio_kbps}k")]);
        }
        pass2
            .args(["-movflags", "+faststart"])
            .overwrite()
            .output(out);
        run(
            &mut pass2,
            duration,
            0.45,
            0.55,
            &format!("Pass 2/2: encoding{retry}"),
            progress,
            cancel,
        )?;

        let size = std::fs::metadata(out)?.len() as f64;
        if size < limit {
            return Ok(());
        }
        target = target * (target0 / size) * 0.95;
    }
    bail!(
        "couldn't get the file under {} MB after several attempts",
        crate::settings::fmt_num(limit_mb)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans() {
        assert_eq!(audio_plan(&[], &[], 100), AudioPlan::None);
        assert_eq!(audio_plan(&[1], &[], 100), AudioPlan::Direct(1));
        assert_eq!(audio_plan(&[1], &[1], 0), AudioPlan::Direct(1)); // strength 0 = off
        let AudioPlan::Graph(g) = audio_plan(&[1], &[1], 40) else {
            panic!()
        };
        assert!(g.starts_with("[0:a:1]arnndn=m=lq.rnnn:mix=0.40,aeval=") && g.ends_with("[aout]"));
        let AudioPlan::Graph(g) = audio_plan(&[0, 1, 2], &[1], 100) else {
            panic!()
        };
        assert!(g.ends_with("[0:a:0][dn1][0:a:2]amix=inputs=3:normalize=0:duration=longest[aout]"));
        let AudioPlan::Graph(g) = audio_plan(&[0, 1], &[], 100) else {
            panic!()
        };
        assert_eq!(
            g,
            "[0:a:0][0:a:1]amix=inputs=2:normalize=0:duration=longest[aout]"
        );
    }
}
