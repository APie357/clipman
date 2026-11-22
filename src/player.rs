// SPDX-License-Identifier: Apache-2.0
//! Preview playback: ffmpeg decodes video frames (scaled for the window) and the
//! selected audio mix as raw PCM, which rodio plays. Both follow one wall clock.

use std::io::Read;
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ffmpeg_sidecar::child::FfmpegChild;
use ffmpeg_sidecar::event::FfmpegEvent;

use crate::render::{self, AudioPlan};

const SAMPLE_RATE: u32 = 48_000;

/// A decoded preview frame (RGB24).
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

enum VideoCmd {
    /// Show the frame at this time (paused / scrubbing).
    Seek(f64),
    /// Stream from this time, pacing frames against `clock` (when `t` should be shown).
    Play(f64, Instant),
    Stop,
}

pub struct Player {
    src: PathBuf,
    duration: f64,
    frame: Arc<Mutex<Option<Frame>>>,
    video_tx: Sender<VideoCmd>,
    /// Some((wall-clock start, media time at start)) while playing.
    clock: Option<(Instant, f64)>,
    paused_at: f64,
    audio: Option<AudioStream>,
}

impl Player {
    pub fn new(
        src: PathBuf,
        duration: f64,
        preview_width: u32,
        ctx: eframe::egui::Context,
    ) -> Self {
        let frame = Arc::new(Mutex::new(None));
        let (video_tx, rx) = mpsc::channel();
        let (f, s) = (frame.clone(), src.clone());
        std::thread::spawn(move || video_thread(s, preview_width, rx, f, ctx));
        let p = Self {
            src,
            duration,
            frame,
            video_tx,
            clock: None,
            paused_at: 0.0,
            audio: None,
        };
        p.video_tx.send(VideoCmd::Seek(0.0)).ok();
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
        self.video_tx.send(VideoCmd::Play(t, clock)).ok();
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
            self.video_tx.send(VideoCmd::Seek(t)).ok();
        }
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
        self.stop_audio();
        self.video_tx.send(VideoCmd::Stop).ok();
    }
}

fn video_cmd(src: &PathBuf, t: f64, width: u32) -> ffmpeg_sidecar::command::FfmpegCommand {
    let mut cmd = render::ffmpeg();
    cmd.seek(format!("{t:.3}")).input(src).map("0:v:0");
    cmd.args(["-vf", &format!("scale={width}:-2:flags=bilinear")]);
    cmd
}

fn kill(mut child: FfmpegChild) {
    let _ = child.kill();
    let _ = child.wait();
}

fn video_thread(
    src: PathBuf,
    width: u32,
    rx: Receiver<VideoCmd>,
    out: Arc<Mutex<Option<Frame>>>,
    ctx: eframe::egui::Context,
) {
    let publish = |f: ffmpeg_sidecar::event::OutputVideoFrame| {
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
            VideoCmd::Seek(t) => {
                let Ok(mut child) = video_cmd(&src, t, width).frames(1).rawvideo().spawn() else {
                    continue;
                };
                let Ok(iter) = child.iter() else { continue };
                for ev in iter {
                    if let FfmpegEvent::OutputFrame(f) = ev {
                        publish(f);
                        break;
                    }
                }
                kill(child);
            }
            VideoCmd::Play(t, clock) => {
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
                    publish(f);
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
        cmd.seek(format!("{t:.3}")).input(src);
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

    /// Plays a real clip for 2 s: `CLIPMAN_TEST_CLIP=clip.mp4 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn plays_real_clip() {
        let clip =
            PathBuf::from(std::env::var("CLIPMAN_TEST_CLIP").expect("set CLIPMAN_TEST_CLIP"));
        let info = crate::media::probe(&clip).unwrap();
        let ctx = eframe::egui::Context::default();
        let mut p = Player::new(clip, info.duration, 960, ctx);
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
