// SPDX-License-Identifier: Apache-2.0
//! Rendering: size-targeted 2-pass x264 (or one-pass hardware H.264, see `gpu`),
//! fixed-quality encodes, lossless trims,
//! audio track mixing and RNNoise noise suppression. Used by the editor (progress shown
//! in its window) and by the command line / right-click menu.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};

use crate::gpu;
use crate::media::{self, parse_ffmpeg_time};
use crate::settings::{Encoder, Preset, cache_dir};

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
    pub encoder: Encoder,
    /// What plugins add (filled in before the job starts; see `plugin::Host::export_start`).
    pub extra: Extra,
}

/// Additions from plugins' `on_export_start`. Plain data, so `render()` never runs Lua.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extra {
    /// Video filters, applied after clipman's scaling (compress presets only).
    pub vf: Vec<String>,
    /// Audio filters, applied after the track mix / noise suppression.
    pub af: Vec<String>,
    /// Ids of the plugins that added filters (for the error on a source preset).
    pub filter_plugins: Vec<String>,
    /// Output options, added to every ffmpeg pass.
    pub output_args: Vec<String>,
    /// Stored in the `clipman` tag as `plugins.<id>`.
    pub meta: serde_json::Map<String, serde_json::Value>,
    /// Plain container tags (`-metadata key=value`).
    pub tags: Vec<(String, String)>,
}

/// Tags clipman writes itself; plugins can't set them.
pub const RESERVED_TAGS: [&str; 2] = ["clipman", "comment"];

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

    /// The same plan with `af` (an ffmpeg audio filter chain) applied to its output.
    /// No audio stays no audio.
    pub fn with_filter(self, af: &str) -> AudioPlan {
        match self {
            AudioPlan::None => AudioPlan::None,
            AudioPlan::Direct(t) => AudioPlan::Graph(format!("[0:a:{t}]{af}[aout]")),
            AudioPlan::Graph(g) => {
                let g = g.strip_suffix("[aout]").unwrap_or(&g);
                AudioPlan::Graph(format!("{g}[apre];[apre]{af}[aout]"))
            }
        }
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
    let mut cmd = FfmpegCommand::new_with_path(media::ffmpeg_bin());
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

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// What's stored in every rendered clip: a `clipman` tag with JSON describing everything
/// needed to make the same clip again from the source file, and a readable `comment`.
/// Only the source's file name is stored, not its path (clips get shared).
struct Meta {
    base: serde_json::Value,
    comment: String,
    tags: Vec<(String, String)>,
}

/// Index of the first frame at or after `t` (frame numbers are 0-based).
fn frame_at(all: &[f64], t: f64) -> usize {
    all.partition_point(|&p| p < t - 0.0005)
}

/// The range to give ffmpeg so it keeps exactly the frames `frame_at(start)..frame_at(end)`:
/// from just before the first kept frame's real timestamp to just before the first frame
/// left out. Cutting at the nominal times instead keeps a frame too many when frames sit
/// a hair off the grid (GSR writes e.g. 4.999999 and 7.999999).
fn exact_span(all: &[f64], start: f64, end: f64, duration: f64) -> (f64, f64) {
    if all.is_empty() {
        return (start, end);
    }
    let (a, b) = (frame_at(all, start), frame_at(all, end));
    let from = all.get(a).map_or(start, |p| p - 0.0005).max(0.0);
    let to = all.get(b).map_or(duration.max(end), |p| p - 0.0005);
    (from, to)
}

fn ffmpeg_version() -> Option<String> {
    let out = media::quiet_command(media::ffmpeg_bin())
        .arg("-version")
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    line.strip_prefix("ffmpeg version ")
        .and_then(|v| v.split_whitespace().next())
        .map(str::to_string)
}

impl Meta {
    /// `start`..`end` is what ends up in the file (for a source preset, `start` is the
    /// keyframe it snapped to; `requested_start` is where the handle was).
    #[allow(clippy::too_many_arguments)]
    fn new(
        job: &Job,
        info: &media::ClipInfo,
        frames: &media::FrameTimes,
        requested_start: f64,
        start: f64,
        end: f64,
        plan: &AudioPlan,
    ) -> Self {
        use serde_json::json;
        let src_name = job
            .src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let size = std::fs::metadata(&job.src).map(|m| m.len()).ok();
        let denoised = |t: &usize| {
            job.strength > 0 && job.denoise.contains(t) && *plan != AudioPlan::None
        };
        let tracks: Vec<_> = job
            .tracks
            .iter()
            .map(|t| {
                json!({
                    "track": t + 1, // as in --audio
                    "name": info.audio_names.get(*t).cloned().flatten(),
                    "denoise": denoised(t),
                })
            })
            .collect();
        let mut preset = json!(job.preset);
        if job.preset.is_original() {
            // height / fps / size don't apply to a stream copy
            for k in ["height", "fps", "max_mb"] {
                preset.as_object_mut().map(|o| o.remove(k));
            }
        }
        let mut audio = json!({ "tracks": tracks });
        if plan.needs_model() {
            audio["denoise_strength"] = json!(job.strength);
            audio["denoise_model"] = json!({ "file": RNNOISE_FILE, "sha256": RNNOISE_SHA256 });
        }
        let mut base = json!({
            "format": 1,
            "clipman": env!("CARGO_PKG_VERSION"),
            "ffmpeg": ffmpeg_version(),
            "source": {
                "name": src_name,
                "size": size,
                "duration": info.duration,
                "width": info.width,
                "height": info.height,
                "fps": info.fps,
            },
            "start": (start * 1e6).round() / 1e6,
            "end": (end * 1e6).round() / 1e6,
            "preset": preset,
            "audio": audio,
        });
        let mut frame_text = String::new();
        if !frames.all.is_empty() {
            let (a, b) = (frame_at(&frames.all, start), frame_at(&frames.all, end));
            // end_frame is exclusive: the clip holds frames start_frame..end_frame
            base["start_frame"] = json!(a);
            base["end_frame"] = json!(b);
            if b > a {
                frame_text = format!(" (frames {a}\u{2013}{})", b - 1);
            }
        }
        if (requested_start - start).abs() > 1e-6 {
            base["requested_start"] = json!((requested_start * 1e6).round() / 1e6);
        }
        if !job.extra.meta.is_empty() {
            base["plugins"] = serde_json::Value::Object(job.extra.meta.clone());
        }
        let comment = format!(
            "Clipman: {src_name}, {}\u{2013}{}{frame_text}, {}",
            media::fmt_time(start),
            media::fmt_time(end),
            job.preset.menu_text()
        );
        let tags = job
            .extra
            .tags
            .iter()
            .filter(|(k, _)| !RESERVED_TAGS.contains(&k.as_str()))
            .cloned()
            .collect();
        Self {
            base,
            comment,
            tags,
        }
    }

    /// Add the metadata (with the encoder details in `video`) and the mp4 flags to the
    /// command writing the final file.
    fn apply(&self, cmd: &mut FfmpegCommand, video: serde_json::Value) {
        let mut m = self.base.clone();
        m["video"] = video;
        // the source's own global tags are just container branding
        cmd.args(["-map_metadata:g", "-1"]);
        cmd.arg("-metadata").arg(format!("clipman={m}"));
        cmd.arg("-metadata").arg(format!("comment={}", self.comment));
        for (k, v) in &self.tags {
            cmd.arg("-metadata").arg(format!("{k}={v}"));
        }
        // use_metadata_tags: mp4 drops custom keys like `clipman` without it
        cmd.args(["-movflags", "+faststart+use_metadata_tags"]);
    }
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
    let x = &job.extra;
    if job.preset.is_original() && !(x.vf.is_empty() && x.af.is_empty()) {
        bail!(
            "plugin filters ({}) can\u{2019}t be applied to a source preset (it copies the \
             video without re-encoding)",
            x.filter_plugins.join(", ")
        );
    }
    let mut plan = audio_plan(&job.tracks, &job.denoise, job.strength);
    if !x.af.is_empty() {
        plan = plan.with_filter(&x.af.join(","));
    }
    if plan.needs_model() {
        progress(0.0, "Getting noise suppression model…");
        ensure_model()?;
    }
    let frames = media::frame_times(&job.src);
    // a lossless cut starts on the keyframe at/before `start` so the copy doesn't begin
    // with frozen frames
    let cut = if job.preset.is_original() {
        media::keyframe_before(&frames.keys, start)
    } else {
        start
    };
    let meta = Meta::new(job, &info, &frames, start, cut, end, &plan);
    if let Some(dir) = job.dst.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("couldn't create {}", dir.display()))?;
    }
    let (from, to) = exact_span(&frames.all, cut, end, info.duration);
    let tmp = TempDir::new()?;
    let out = tmp.0.join("out.mp4");
    if job.preset.is_original() {
        render_original(job, cut, to, &plan, &meta, &out, progress, cancel)?;
    } else {
        render_compress(
            job, &info, from, to, &plan, &meta, &tmp.0, &out, progress, cancel,
        )?;
    }
    move_file(&out, &job.dst)?;
    let size = std::fs::metadata(&job.dst)?.len();
    progress(1.0, "Done");
    Ok((job.dst.clone(), size))
}

fn trim(cmd: &mut FfmpegCommand, start: f64, dur: f64) {
    // 6 decimals: rounding to ms would move the cut across frames that are a hair off
    cmd.seek(format!("{start:.6}"))
        .duration(format!("{dur:.6}"));
}

/// Lossless video cut from keyframe `k`.
#[allow(clippy::too_many_arguments)]
fn render_original(
    job: &Job,
    k: f64,
    end: f64,
    plan: &AudioPlan,
    meta: &Meta,
    out: &Path,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut cmd = ffmpeg();
    // seek just past the keyframe (copy starts on the keyframe at/before it); -t
    // counts from the seek point, not from the keyframe
    let seek = k + 0.001;
    cmd.seek(format!("{seek:.6}"))
        .input(&job.src)
        .duration(format!("{:.6}", end - seek));
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
    cmd.args(["-avoid_negative_ts", "make_zero"]);
    cmd.args(&job.extra.output_args);
    meta.apply(&mut cmd, serde_json::json!({ "encoder": "copy" }));
    cmd.overwrite().output(out);
    run(
        &mut cmd,
        end - k,
        0.0,
        1.0,
        "Trimming (source)",
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
    meta: &Meta,
    tmp: &Path,
    out: &Path,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<()> {
    let hw = if job.encoder.uses_gpu(job.preset.limit_mb().is_some()) {
        progress(0.0, "Looking for a GPU encoder…");
        gpu::detect()
    } else {
        None
    };
    if let Some(hw) = hw {
        match encode(job, info, start, end, plan, meta, tmp, out, Some(hw), progress, cancel) {
            Err(e) if !e.is::<Cancelled>() => {
                // e.g. a driver problem or a source the GPU path can't take: x264 still works
                eprintln!("clipman: GPU encode ({}) failed, using the CPU: {e:#}", hw.codec());
            }
            r => return r,
        }
    }
    encode(job, info, start, end, plan, meta, tmp, out, None, progress, cancel)
}

/// One compress attempt with x264 (`hw` None) or a hardware encoder.
#[allow(clippy::too_many_arguments)]
fn encode(
    job: &Job,
    info: &media::ClipInfo,
    start: f64,
    end: f64,
    plan: &AudioPlan,
    meta: &Meta,
    tmp: &Path,
    out: &Path,
    hw: Option<&gpu::HwEncoder>,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<()> {
    let p = &job.preset;
    let duration = end - start;
    let trimmed = start > 0.0 || end < info.duration;
    let mut vf = format!("scale=-2:'min({},ih)':flags=lanczos", p.height); // never upscale
    for f in &job.extra.vf {
        vf += ",";
        vf += f; // software filters, so before the GPU upload
    }
    if let Some(hw) = hw {
        vf += hw.filter_suffix();
    }
    let on = if hw.is_some() { " on GPU" } else { "" };
    // input, scaling and frame rate (codec / rate control are added per mode)
    let base = |cmd: &mut FfmpegCommand| {
        if let Some(hw) = hw {
            hw.input_args(cmd);
        }
        if trimmed {
            trim(cmd, start, duration);
        }
        cmd.input(&job.src).map("0:v:0").args(["-vf", vf.as_str()]);
        cmd.args(["-fpsmax", &p.fps.to_string()]);
        cmd.args(&job.extra.output_args);
    };
    let x264 = |cmd: &mut FfmpegCommand| {
        cmd.codec_video("libx264").preset("slow").pix_fmt("yuv420p");
    };
    let codec = hw.map_or("libx264", |h| h.codec());
    let finish = |cmd: &mut FfmpegCommand, audio_kbps: u32, mut video: serde_json::Value| {
        plan.apply(cmd);
        if *plan != AudioPlan::None {
            cmd.codec_audio("aac")
                .args(["-b:a", &format!("{audio_kbps}k")]);
            video["audio_kbps"] = audio_kbps.into();
        }
        video["encoder"] = codec.into();
        video["filter"] = vf.as_str().into();
        video["fpsmax"] = p.fps.into();
        meta.apply(cmd, video);
        cmd.overwrite().output(out);
    };

    let Some(limit_mb) = p.limit_mb() else {
        // no size limit: one pass at a fixed quality
        let mut cmd = ffmpeg();
        base(&mut cmd);
        let video = match hw {
            Some(hw) => {
                hw.quality_args(&mut cmd);
                serde_json::json!({ "qp": gpu::QUALITY_QP })
            }
            None => {
                x264(&mut cmd);
                cmd.crf(NO_LIMIT_CRF);
                serde_json::json!({ "preset": "slow", "crf": NO_LIMIT_CRF })
            }
        };
        finish(&mut cmd, NO_LIMIT_AUDIO_KBPS, video);
        return run(
            &mut cmd,
            duration,
            0.0,
            1.0,
            &format!("Encoding{on} (no size limit)"),
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
        let retry = if attempt == 1 {
            String::new()
        } else {
            format!(" (retry {})", attempt - 1)
        };

        if let Some(hw) = hw {
            // hardware encoders have no 2-pass mode: one pass at the average bitrate,
            // and the size check below retries if it overshoots
            let mut cmd = ffmpeg();
            base(&mut cmd);
            hw.bitrate_args(&mut cmd, v_kbps);
            let video = serde_json::json!({ "passes": 1, "video_kbps": v_kbps, "attempt": attempt });
            finish(&mut cmd, audio_kbps, video);
            run(
                &mut cmd,
                duration,
                0.0,
                1.0,
                &format!("Encoding{on}{retry}"),
                progress,
                cancel,
            )?;
        } else {
            let rate = |cmd: &mut FfmpegCommand| {
                x264(cmd);
                cmd.args([
                    "-b:v",
                    &format!("{v_kbps}k"),
                    "-maxrate",
                    &format!("{}k", v_kbps * 3 / 2),
                ]);
                cmd.args(["-bufsize", &format!("{}k", v_kbps * 2)]);
                cmd.args(["-passlogfile"]).arg(&passlog);
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
            let video = serde_json::json!({
                "preset": "slow",
                "passes": 2,
                "video_kbps": v_kbps,
                "attempt": attempt,
            });
            finish(&mut pass2, audio_kbps, video);
            run(
                &mut pass2,
                duration,
                0.45,
                0.55,
                &format!("Pass 2/2: encoding{retry}"),
                progress,
                cancel,
            )?;
        }

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
    fn exact_frames() {
        // GSR-style timestamps, a hair off the 60 fps grid
        let all: Vec<f64> = (0..600)
            .map(|i| i as f64 / 60.0 - if i % 60 == 0 { 1e-6 } else { 0.0 })
            .map(|t: f64| t.max(0.0))
            .collect();
        assert_eq!(frame_at(&all, 5.0), 300); // 4.999999 counts as the frame at 5 s
        assert_eq!(frame_at(&all, 5.01), 301);
        let (from, to) = exact_span(&all, 5.0, 8.0, 10.0);
        assert!(from < all[300] && from > all[299]);
        assert!(to < all[480] && to > all[479]); // 480 (7.999999) is left out
        assert_eq!(all.iter().filter(|&&p| p >= from && p < to).count(), 180);
        // to the end of the clip: the clip's duration
        assert_eq!(exact_span(&all, 9.0, 10.0, 10.0).1, 10.0);
        // no frame info: the times as given
        assert_eq!(exact_span(&[], 1.0, 2.0, 10.0), (1.0, 2.0));
    }

    #[test]
    fn plugin_audio_filter() {
        assert_eq!(audio_plan(&[], &[], 100).with_filter("volume=2"), AudioPlan::None);
        assert_eq!(
            AudioPlan::Direct(1).with_filter("volume=2"),
            AudioPlan::Graph("[0:a:1]volume=2[aout]".into())
        );
        let AudioPlan::Graph(g) = audio_plan(&[0, 1], &[], 100).with_filter("volume=2") else {
            panic!()
        };
        assert_eq!(
            g,
            "[0:a:0][0:a:1]amix=inputs=2:normalize=0:duration=longest[apre];[apre]volume=2[aout]"
        );
    }

    #[test]
    fn plugin_metadata() {
        let mut job = Job {
            src: "/clips/a.mp4".into(),
            dst: "/out/a.mp4".into(),
            preset: crate::settings::default_presets().remove(0),
            start: None,
            end: None,
            tracks: vec![],
            denoise: vec![],
            strength: 100,
            encoder: Encoder::Cpu,
            extra: Extra::default(),
        };
        job.extra.meta.insert("tags".into(), serde_json::json!({ "game": "x" }));
        job.extra.tags = vec![("title".into(), "Hi".into()), ("comment".into(), "no".into())];
        let info = media::ClipInfo::default();
        let m = Meta::new(&job, &info, &Default::default(), 0.0, 0.0, 1.0, &AudioPlan::None);
        assert_eq!(m.base["plugins"]["tags"]["game"], "x");
        assert_eq!(m.tags, [("title".to_string(), "Hi".to_string())]); // comment is ours
    }

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
