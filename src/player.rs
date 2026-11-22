// SPDX-License-Identifier: Apache-2.0
//! Preview playback: ffmpeg decodes video frames (scaled for the window) and the
//! selected audio mix as raw PCM, which rodio plays. Both follow one wall clock.

use std::io::Read;
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ffmpeg_sidecar::child::FfmpegChild;
use ffmpeg_sidecar::event::FfmpegEvent;

use crate::render::{self, AudioPlan};

const SAMPLE_RATE: u32 = 48_000;
/// Scrub preview strip: small frames decoded in the background so dragging the playhead
/// shows something instantly; the exact frame follows once the playhead rests.
/// 480p, kept as JPEG in memory (~50 KB a frame) and decoded only when shown.
const SCRUB_WIDTH: u32 = 854;
const SCRUB_FPS: f64 = 10.0;
const SCRUB_MAX_FRAMES: f64 = 600.0; // ~30-40 MB of JPEG, whatever the clip length
/// Exact seeks decode a window of frames around the target and keep them, so stepping
/// frame by frame nearby is instant. Biased forward, the usual stepping direction
/// (13 frames at 1280x720 ≈ 36 MB).
const WINDOW_BEFORE: usize = 3;
const WINDOW_AFTER: usize = 9;
/// Wait this long for the playhead to settle before decoding the exact frame.
const SEEK_SETTLE: Duration = Duration::from_millis(40);

/// A decoded preview frame (RGB24).
#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

/// Commands carry a generation number; frames from an older generation are dropped, so
/// a slow decode for an old position can never overwrite a newer one.
enum VideoCmd {
    /// Show the exact frame at this time (paused / scrubbing).
    Seek(f64, u64),
    /// Stream from this time, pacing frames against `clock` (when `t` should be shown).
    Play(f64, Instant, u64),
    Stop,
}

/// Low-res frames every `step` seconds, filled in by a background decode.
struct ScrubStrip {
    step: f64,
    /// JPEG-encoded frames, one every `step` seconds.
    frames: Vec<Vec<u8>>,
}

pub struct Player {
    src: PathBuf,
    duration: f64,
    frame: Arc<Mutex<Option<Frame>>>,
    video_tx: Sender<VideoCmd>,
    generation: Arc<AtomicU64>,
    strip: Arc<Mutex<ScrubStrip>>,
    /// Exactly decoded frames around the last exact seek: (timestamp, frame).
    window: Arc<Mutex<Vec<(f64, Frame)>>>,
    alive: Arc<AtomicBool>,
    ctx: eframe::egui::Context,
    /// Some((wall-clock start, media time at start)) while playing.
    clock: Option<(Instant, f64)>,
    paused_at: f64,
    audio: Option<AudioStream>,
}

impl Player {
    pub fn new(
        src: PathBuf,
        duration: f64,
        fps: f64,
        preview_width: u32,
        ctx: eframe::egui::Context,
    ) -> Self {
        let frame = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let step = (1.0 / SCRUB_FPS).max(duration / SCRUB_MAX_FRAMES);
        let window = Arc::new(Mutex::new(Vec::new()));
        let strip = Arc::new(Mutex::new(ScrubStrip {
            step,
            frames: Vec::new(),
        }));
        let (video_tx, rx) = mpsc::channel();
        {
            let (f, s, g, c) = (frame.clone(), src.clone(), generation.clone(), ctx.clone());
            let w = window.clone();
            let frame_dur = if fps > 1.0 { 1.0 / fps } else { 1.0 / 60.0 };
            std::thread::spawn(move || {
                video_thread(s, duration, frame_dur, preview_width, rx, f, w, g, c)
            });
            let (s, st, a) = (src.clone(), strip.clone(), alive.clone());
            std::thread::spawn(move || strip_thread(s, step, st, a));
        }
        let mut p = Self {
            src,
            duration,
            frame,
            video_tx,
            generation,
            strip,
            window,
            alive,
            ctx,
            clock: None,
            paused_at: 0.0,
            audio: None,
        };
        let g = p.next_generation();
        p.video_tx.send(VideoCmd::Seek(0.0, g)).ok();
        p
    }

    pub fn position(&self) -> f64 {
        match self.clock {
            Some((start, from)) => (from + start.elapsed().as_secs_f64()).min(self.duration),
            None => self.paused_at,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.clock.is_some()
    }

    /// Play from `t` with the given audio (None = silent preview).
    pub fn play(&mut self, t: f64, audio: Option<(&rodio::mixer::Mixer, &AudioPlan)>) {
        self.stop_audio();
        // small lead so both ffmpeg processes have started before the clock reaches `t`
        let clock = Instant::now() + Duration::from_millis(150);
        self.clock = Some((clock, t));
        let g = self.next_generation();
        self.video_tx.send(VideoCmd::Play(t, clock, g)).ok();
        if let Some((mixer, plan)) = audio {
            self.audio = AudioStream::start(&self.src, t, plan, mixer, clock);
        }
    }

    /// Restart audio at the current position (tracks or noise suppression changed).
    pub fn restart_audio(&mut self, mixer: &rodio::mixer::Mixer, plan: &AudioPlan) {
        if self.is_playing() {
            let t = self.position();
            self.play(t, Some((mixer, plan)));
        }
    }

    pub fn pause(&mut self) {
        if self.clock.is_some() {
            self.paused_at = self.position();
            self.clock = None;
            self.video_tx.send(VideoCmd::Stop).ok();
            self.stop_audio();
        }
    }

    /// Jump to `t` (shows that frame; keeps playing if playing).
    pub fn seek(&mut self, t: f64, audio: Option<(&rodio::mixer::Mixer, &AudioPlan)>) {
        let t = t.clamp(0.0, self.duration);
        if self.is_playing() {
            self.play(t, audio);
        } else {
            self.paused_at = t;
            let g = self.next_generation();
            if !self.show_window_frame(t) {
                self.show_strip_frame(t);
                self.video_tx.send(VideoCmd::Seek(t, g)).ok();
            }
        }
    }

    /// Show the exact frame at timestamp `t` (frame stepping): instant when it's in the
    /// decoded window, otherwise decoded without flashing the low-res strip in between.
    pub fn step_to(&mut self, t: f64, audio: Option<(&rodio::mixer::Mixer, &AudioPlan)>) {
        let t = t.clamp(0.0, self.duration);
        if self.is_playing() {
            return self.play(t, audio);
        }
        self.paused_at = t;
        let g = self.next_generation();
        if !self.show_window_frame(t) {
            self.video_tx.send(VideoCmd::Seek(t, g)).ok();
        }
    }

    /// Publish the frame at `t` from the decoded window, if it's there.
    fn show_window_frame(&self, t: f64) -> bool {
        let Ok(win) = self.window.lock() else {
            return false;
        };
        let Some((_, f)) = win.iter().find(|(p, _)| (p - t).abs() < 0.004) else {
            return false;
        };
        if let Ok(mut slot) = self.frame.lock() {
            *slot = Some(f.clone());
        }
        self.ctx.request_repaint();
        true
    }

    fn next_generation(&mut self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Show the nearest scrub-strip frame right away (the exact frame replaces it).
    fn show_strip_frame(&self, t: f64) {
        let jpeg = {
            let Ok(strip) = self.strip.lock() else { return };
            let i = (t / strip.step).round() as usize;
            match strip
                .frames
                .get(i.min(strip.frames.len().saturating_sub(1)))
            {
                Some(j) => j.clone(),
                None => return,
            }
        };
        let Ok(img) = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg) else {
            return;
        };
        let img = img.to_rgb8();
        if let Ok(mut slot) = self.frame.lock() {
            *slot = Some(Frame {
                width: img.width() as usize,
                height: img.height() as usize,
                rgb: img.into_raw(),
            });
        }
        self.ctx.request_repaint();
    }

    /// The newest decoded frame, if it changed since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.frame.lock().ok()?.take()
    }

    fn stop_audio(&mut self) {
        if let Some(a) = self.audio.take() {
            a.stop();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        self.stop_audio();
        self.video_tx.send(VideoCmd::Stop).ok();
    }
}

/// Decode the scrub strip: a frame every `step` seconds, SCRUB_WIDTH wide, encoded to
/// JPEG by ffmpeg (an MJPEG stream on stdout, split into frames at the end-of-image marker).
fn strip_thread(src: PathBuf, step: f64, strip: Arc<Mutex<ScrubStrip>>, alive: Arc<AtomicBool>) {
    let mut cmd = render::ffmpeg();
    cmd.input(&src).map("0:v:0");
    cmd.args([
        "-vf",
        &format!("fps=1/{step:.4},scale={SCRUB_WIDTH}:-2:flags=bicubic"),
    ]);
    cmd.args(["-c:v", "mjpeg", "-q:v", "4", "-f", "image2pipe"])
        .pipe_stdout();
    let Ok(mut child) = cmd.spawn() else { return };
    let Some(mut stdout) = child.take_stdout() else {
        return;
    };
    if let Some(mut err) = child.take_stderr() {
        std::thread::spawn(move || std::io::copy(&mut err, &mut std::io::sink()));
    }
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    while alive.load(Ordering::Relaxed) {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let scan_from = buf.len().saturating_sub(1);
        buf.extend_from_slice(&chunk[..n]);
        // FF D9 = end of image; it can't occur inside JPEG data (FF bytes are stuffed)
        let mut start = 0;
        let mut i = scan_from;
        while i + 1 < buf.len() {
            if buf[i] == 0xFF && buf[i + 1] == 0xD9 {
                let frame = buf[start..i + 2].to_vec();
                if let Ok(mut s) = strip.lock() {
                    s.frames.push(frame);
                }
                start = i + 2;
                i += 2;
            } else {
                i += 1;
            }
        }
        buf.drain(..start);
    }
    kill(child);
}

fn video_cmd(src: &PathBuf, t: f64, width: u32) -> ffmpeg_sidecar::command::FfmpegCommand {
    let mut cmd = render::ffmpeg();
    cmd.seek(format!("{t:.6}")).input(src).map("0:v:0");
    cmd.args(["-vf", &format!("scale={width}:-2:flags=bilinear")]);
    cmd
}

fn kill(mut child: FfmpegChild) {
    let _ = child.kill();
    let _ = child.wait();
}

#[allow(clippy::too_many_arguments)]
fn video_thread(
    src: PathBuf,
    duration: f64,
    frame_dur: f64,
    width: u32,
    rx: Receiver<VideoCmd>,
    out: Arc<Mutex<Option<Frame>>>,
    window: Arc<Mutex<Vec<(f64, Frame)>>>,
    generation: Arc<AtomicU64>,
    ctx: eframe::egui::Context,
) {
    let publish_frame = |f: Frame, g: u64| {
        if generation.load(Ordering::SeqCst) != g {
            return;
        }
        if let Ok(mut slot) = out.lock() {
            *slot = Some(f);
        }
        ctx.request_repaint();
    };
    let publish = |f: ffmpeg_sidecar::event::OutputVideoFrame, g: u64| {
        if generation.load(Ordering::SeqCst) != g {
            return; // a newer seek/play superseded this one
        }
        if let Ok(mut slot) = out.lock() {
            *slot = Some(Frame {
                width: f.width as usize,
                height: f.height as usize,
                rgb: f.data,
            });
        }
        ctx.request_repaint();
    };
    let mut pending: Option<VideoCmd> = None;
    loop {
        let mut cmd = match pending.take() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => return,
            },
        };
        // only the newest request matters (scrubbing sends many)
        while let Ok(c) = rx.try_recv() {
            cmd = c;
        }
        match cmd {
            VideoCmd::Stop => {}
            VideoCmd::Seek(t, g) => {
                // let the playhead settle (the scrub strip covers the meantime)
                match rx.recv_timeout(SEEK_SETTLE) {
                    Ok(c) => {
                        pending = Some(c);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                // There's no frame at exactly `duration`, so seeking to the end used to
                // decode nothing; stay a little before it and step back if still empty.
                let mut at = t.min(duration - 0.05).max(0.0);
                for _attempt in 0..3 {
                    // decode a window of frames around the target and keep them, so the
                    // next frame steps nearby don't need ffmpeg at all
                    let from = (at - WINDOW_BEFORE as f64 * frame_dur).max(0.0);
                    let count = (WINDOW_BEFORE + 1 + WINDOW_AFTER) as u32;
                    let Ok(mut child) = video_cmd(&src, from, width)
                        .frames(count)
                        .rawvideo()
                        .spawn()
                    else {
                        break;
                    };
                    let Ok(iter) = child.iter() else { break };
                    let mut got = false;
                    let mut frames: Vec<(f64, Frame)> = Vec::new();
                    for ev in iter {
                        // a newer request cancels this decode
                        if let Ok(c) = rx.try_recv() {
                            pending = Some(c);
                            break;
                        }
                        let FfmpegEvent::OutputFrame(f) = ev else {
                            continue;
                        };
                        let pts = from + f.timestamp as f64;
                        let frame = Frame {
                            width: f.width as usize,
                            height: f.height as usize,
                            rgb: f.data,
                        };
                        // show the target as soon as it's decoded (first frame at/after `at`)
                        if !got && pts >= at - 0.0005 {
                            publish_frame(frame.clone(), g);
                            got = true;
                        }
                        frames.push((pts, frame));
                        if frames.len() as u32 >= count {
                            break;
                        }
                    }
                    kill(child);
                    if !got && let Some((_, last)) = frames.last() {
                        publish_frame(last.clone(), g); // near the end: the last frame there is
                        got = true;
                    }
                    if !frames.is_empty()
                        && let Ok(mut w) = window.lock()
                    {
                        *w = frames;
                    }
                    if got || pending.is_some() || at <= 0.0 {
                        break;
                    }
                    at = (at - 0.5).max(0.0);
                }
            }
            VideoCmd::Play(t, clock, g) => {
                let Ok(mut child) = video_cmd(&src, t, width).rawvideo().spawn() else {
                    continue;
                };
                let Ok(iter) = child.iter() else { continue };
                for ev in iter {
                    if let Ok(c) = rx.try_recv() {
                        pending = Some(c);
                        break;
                    }
                    let FfmpegEvent::OutputFrame(f) = ev else {
                        continue;
                    };
                    let due = clock + Duration::from_secs_f64(f.timestamp.max(0.0) as f64);
                    let now = Instant::now();
                    if due > now {
                        // wait, but stay responsive to new commands
                        match rx.recv_timeout(due - now) {
                            Ok(c) => {
                                pending = Some(c);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    } else if now - due > Duration::from_millis(80) {
                        continue; // running late: drop this frame
                    }
                    publish(f, g);
                }
                kill(child);
            }
        }
    }
}

/// The selected audio mix decoded by ffmpeg to f32 PCM and played by rodio.
struct AudioStream {
    child: FfmpegChild,
    player: rodio::Player,
}

impl AudioStream {
    fn start(
        src: &PathBuf,
        t: f64,
        plan: &AudioPlan,
        mixer: &rodio::mixer::Mixer,
        clock: Instant,
    ) -> Option<Self> {
        if *plan == AudioPlan::None {
            return None;
        }
        let mut cmd = render::ffmpeg();
        cmd.seek(format!("{t:.6}")).input(src);
        plan.apply(&mut cmd);
        cmd.args(["-f", "f32le", "-ar", &SAMPLE_RATE.to_string(), "-ac", "2"])
            .pipe_stdout();
        let mut child = cmd.spawn().ok()?;
        let mut stdout = child.take_stdout()?;
        // drain stderr so ffmpeg never blocks on a full pipe
        if let Some(mut err) = child.take_stderr() {
            std::thread::spawn(move || std::io::copy(&mut err, &mut std::io::sink()));
        }
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(64);
        std::thread::spawn(move || pump(&mut stdout, tx));
        let player = rodio::Player::connect_new(mixer);
        // silence until the shared clock starts, so audio lines up with the video
        player.append(PcmSource {
            rx,
            buf: Vec::new(),
            pos: 0,
            start_at: clock,
        });
        Some(Self { child, player })
    }

    fn stop(self) {
        self.player.stop();
        kill(self.child);
    }
}

fn pump(stdout: &mut impl Read, tx: SyncSender<Vec<f32>>) {
    let mut bytes = vec![0u8; 4096 * 4];
    let mut carry = Vec::new();
    loop {
        let n = match stdout.read(&mut bytes) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        carry.extend_from_slice(&bytes[..n]);
        let whole = carry.len() / 4 * 4;
        let samples = carry[..whole]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        carry.drain(..whole);
        if tx.send(samples).is_err() {
            return;
        }
    }
}

struct PcmSource {
    rx: Receiver<Vec<f32>>,
    buf: Vec<f32>,
    pos: usize,
    start_at: Instant,
}

impl Iterator for PcmSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if Instant::now() < self.start_at {
            return Some(0.0);
        }
        if self.pos >= self.buf.len() {
            // runs in the audio callback: never block; output silence when starved
            match self.rx.try_recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(mpsc::TryRecvError::Empty) => return Some(0.0),
                Err(mpsc::TryRecvError::Disconnected) => return None,
            }
            if self.buf.is_empty() {
                return Some(0.0);
            }
        }
        self.pos += 1;
        Some(self.buf[self.pos - 1])
    }
}

impl rodio::Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> rodio::ChannelCount {
        NonZero::new(2).unwrap()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        NonZero::new(SAMPLE_RATE).unwrap()
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// The audio output device, opened once per app (None when there's no audio device).
pub fn open_audio() -> Option<rodio::MixerDeviceSink> {
    let mut sink = rodio::DeviceSinkBuilder::open_default_sink().ok()?;
    sink.log_on_drop(false);
    Some(sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scrub latency on a real clip: `CLIPMAN_TEST_CLIP=clip.mp4 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn scrubbing_is_fast() {
        let clip =
            PathBuf::from(std::env::var("CLIPMAN_TEST_CLIP").expect("set CLIPMAN_TEST_CLIP"));
        let info = crate::media::probe(&clip).unwrap();
        let mut p = Player::new(
            clip,
            info.duration,
            info.fps,
            1280,
            eframe::egui::Context::default(),
        );
        let wait_frame = |p: &Player, max_ms: u64| -> Option<(Duration, usize)> {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(max_ms) {
                if let Some(f) = p.take_frame() {
                    return Some((t0.elapsed(), f.width));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            None
        };
        wait_frame(&p, 3000).expect("first frame");
        // before the strip exists: exact frame only
        p.seek(31.9, None);
        eprintln!("cold seek, exact frame: {:?}", wait_frame(&p, 3000));
        let t0 = Instant::now();
        while p.strip.lock().unwrap().frames.len() < (info.duration * SCRUB_FPS) as usize - 2 {
            assert!(t0.elapsed() < Duration::from_secs(30), "strip too slow");
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!(
            "scrub strip ready after {:.1}s: {} frames",
            t0.elapsed().as_secs_f64(),
            p.strip.lock().unwrap().frames.len()
        );
        // simulate a drag: 30 positions at 60 Hz, then rest
        for k in 0..30 {
            p.seek(10.0 + k as f64 * 0.5, None);
            let (lat, w) = wait_frame(&p, 100).expect("no preview during drag");
            assert!(
                lat < Duration::from_millis(25) && w == SCRUB_WIDTH as usize,
                "drag frame {lat:?} {w}"
            );
            std::thread::sleep(Duration::from_millis(16));
        }
        let (lat, w) = wait_frame(&p, 2000).expect("exact frame after drag");
        eprintln!("drag: preview frames instant; exact {w}px frame {lat:?} after the last move");
        assert_eq!(w, 1280);
        // the very end: there's no frame at exactly `duration`; must still get a sharp one
        p.seek(info.duration, None);
        let mut sizes = vec![];
        while let Some((_, w)) = wait_frame(&p, 2000) {
            sizes.push(w);
            if w == 1280 {
                break;
            }
        }
        eprintln!("seek to end: frames shown {sizes:?}");
        assert_eq!(sizes.last(), Some(&1280), "end frame stayed low-res");
    }

    /// Frame stepping shows exactly frame N: `CLIPMAN_TEST_CLIP=clip.mp4 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn frame_steps_are_exact() {
        let clip =
            PathBuf::from(std::env::var("CLIPMAN_TEST_CLIP").expect("set CLIPMAN_TEST_CLIP"));
        let info = crate::media::probe(&clip).unwrap();
        let ft = crate::media::frame_times(&clip);
        let mut p = Player::new(
            clip.clone(),
            info.duration,
            info.fps,
            1280,
            eframe::egui::Context::default(),
        );
        let wait = |p: &Player| {
            let t0 = Instant::now();
            loop {
                if let Some(f) = p.take_frame() {
                    return (t0.elapsed(), f);
                }
                assert!(t0.elapsed() < Duration::from_secs(3), "no frame");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait(&p);
        // reference: frame N decoded by number, same scaling
        let reference = |n: usize| -> Vec<u8> {
            let out = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&clip)
                .args([
                    "-map",
                    "0:v:0",
                    "-vf",
                    &format!("select=eq(n\\,{n}),scale=1280:-2:flags=bilinear"),
                ])
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
                .output()
                .unwrap();
            out.stdout
        };
        let mut lat = vec![];
        for n in 300..312 {
            p.step_to(ft.all[n], None);
            let (l, f) = wait(&p);
            lat.push(l);
            let r = reference(n);
            assert_eq!(f.rgb.len(), r.len());
            let diff = f
                .rgb
                .iter()
                .zip(&r)
                .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
                .sum::<u64>() as f64
                / r.len() as f64;
            assert!(diff < 2.0, "frame {n}: mean pixel difference {diff:.2}");
            // and it isn't also identical to a neighbour (the step really moved one frame)
            let prev = reference(n - 1);
            let d_prev = f
                .rgb
                .iter()
                .zip(&prev)
                .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
                .sum::<u64>() as f64
                / r.len() as f64;
            eprintln!(
                "frame {n}: diff to reference {diff:.3}, to previous frame {d_prev:.3}, shown after {l:?}"
            );
        }
        let instant = lat
            .iter()
            .filter(|l| **l < Duration::from_millis(5))
            .count();
        eprintln!(
            "{instant}/{} steps served from the decoded window instantly",
            lat.len()
        );
    }

    /// Plays a real clip for 2 s: `CLIPMAN_TEST_CLIP=clip.mp4 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn plays_real_clip() {
        let clip =
            PathBuf::from(std::env::var("CLIPMAN_TEST_CLIP").expect("set CLIPMAN_TEST_CLIP"));
        let info = crate::media::probe(&clip).unwrap();
        let ctx = eframe::egui::Context::default();
        let mut p = Player::new(clip, info.duration, info.fps, 960, ctx);
        let first = (0..100).find_map(|_| {
            std::thread::sleep(Duration::from_millis(20));
            p.take_frame()
        });
        let f = first.expect("no paused frame");
        assert_eq!((f.width, f.rgb.len()), (960, f.width * f.height * 3));

        let sink = open_audio();
        let plan = crate::render::audio_plan(&[0, 1], &[], 100);
        p.play(10.0, sink.as_ref().map(|s| (s.mixer(), &plan)));
        let started = Instant::now();
        let mut frames = 0;
        while started.elapsed() < Duration::from_secs(2) {
            if p.take_frame().is_some() {
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let pos = p.position();
        let audio_running = p
            .audio
            .as_mut()
            .map(|a| a.child.as_inner_mut().try_wait().unwrap().is_none());
        p.pause();
        eprintln!(
            "frames shown in 2 s: {frames}, position {pos:.2}, audio device: {}, audio ffmpeg running: {audio_running:?}",
            sink.is_some()
        );
        assert!(frames > 60, "too few frames: {frames}");
        assert!((pos - 11.85).abs() < 0.1, "clock off: {pos}");
    }
}
