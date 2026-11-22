// SPDX-License-Identifier: Apache-2.0
//! Hardware H.264 encoders: finding one that works (a tiny test encode per candidate,
//! once per process) and the ffmpeg options each one needs.

use std::path::PathBuf;
use std::sync::OnceLock;

use ffmpeg_sidecar::command::FfmpegCommand;

/// Constant quality used when a preset has no size limit. Hardware QP isn't CRF: on
/// VAAPI (RX 7900), QP 24 scores like x264 CRF 20 (VMAF ~98) at ~1.5x the size.
pub const QUALITY_QP: u32 = 24;

#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    /// Linux VAAPI (AMD, Intel) on a DRM render node.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Vaapi(PathBuf),
    /// NVIDIA.
    Nvenc,
    /// AMD on Windows.
    #[cfg_attr(not(windows), allow(dead_code))]
    Amf,
    /// Intel Quick Sync on Windows.
    #[cfg_attr(not(windows), allow(dead_code))]
    Qsv,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HwEncoder {
    pub backend: Backend,
}

impl HwEncoder {
    pub fn codec(&self) -> &'static str {
        match self.backend {
            Backend::Vaapi(_) => "h264_vaapi",
            Backend::Nvenc => "h264_nvenc",
            Backend::Amf => "h264_amf",
            Backend::Qsv => "h264_qsv",
        }
    }

    /// For the settings dialog, e.g. "h264_vaapi on renderD128".
    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::Vaapi(dev) => format!(
                "{} on {}",
                self.codec(),
                dev.file_name().unwrap_or_default().to_string_lossy()
            ),
            _ => self.codec().to_string(),
        }
    }

    /// Options that must come before the input.
    pub fn input_args(&self, cmd: &mut FfmpegCommand) {
        if let Backend::Vaapi(dev) = &self.backend {
            cmd.arg("-vaapi_device").arg(dev);
        }
    }

    /// Appended to the (CPU) video filter chain: get frames into the encoder's format.
    pub fn filter_suffix(&self) -> &'static str {
        match self.backend {
            Backend::Vaapi(_) => ",format=nv12,hwupload",
            Backend::Qsv => ",format=nv12",
            Backend::Nvenc | Backend::Amf => ",format=yuv420p",
        }
    }

    /// Codec + speed/quality options shared by both rate modes.
    fn codec_args(&self, cmd: &mut FfmpegCommand) {
        cmd.codec_video(self.codec());
        match self.backend {
            Backend::Vaapi(_) => {}
            Backend::Nvenc => {
                cmd.args(["-preset", "p6", "-tune", "hq"]);
            }
            Backend::Amf => {
                cmd.args(["-quality", "quality"]);
            }
            Backend::Qsv => {
                cmd.args(["-preset", "slow"]);
            }
        }
    }

    /// Constant quality (presets without a size limit).
    pub fn quality_args(&self, cmd: &mut FfmpegCommand) {
        self.codec_args(cmd);
        let qp = QUALITY_QP.to_string();
        match self.backend {
            Backend::Vaapi(_) => cmd.args(["-rc_mode", "CQP", "-qp", &qp]),
            Backend::Nvenc => cmd.args(["-rc", "vbr", "-cq", &qp, "-b:v", "0"]),
            Backend::Amf => cmd.args(["-rc", "cqp", "-qp_i", &qp, "-qp_p", &qp]),
            Backend::Qsv => cmd.args(["-global_quality", &qp]),
        };
    }

    /// Average bitrate with a peak (size-limited presets; there is no 2-pass).
    pub fn bitrate_args(&self, cmd: &mut FfmpegCommand, kbps: u32) {
        self.codec_args(cmd);
        match self.backend {
            Backend::Vaapi(_) => {
                cmd.args(["-rc_mode", "VBR"]);
            }
            Backend::Nvenc => {
                cmd.args(["-rc", "vbr", "-multipass", "fullres"]);
            }
            Backend::Amf => {
                cmd.args(["-rc", "vbr_peak"]);
            }
            Backend::Qsv => {}
        }
        cmd.args([
            "-b:v",
            &format!("{kbps}k"),
            "-maxrate",
            &format!("{}k", kbps * 3 / 2),
            "-bufsize",
            &format!("{}k", kbps * 2),
        ]);
    }
}

/// Encoders worth trying on this platform, most preferred first.
fn candidates() -> Vec<HwEncoder> {
    #[allow(unused_mut)]
    let mut c = vec![HwEncoder {
        backend: Backend::Nvenc,
    }];
    #[cfg(target_os = "linux")]
    c.extend(render_nodes().into_iter().map(|d| HwEncoder {
        backend: Backend::Vaapi(d),
    }));
    #[cfg(windows)]
    c.extend([Backend::Amf, Backend::Qsv].map(|backend| HwEncoder { backend }));
    c
}

/// DRM render nodes, the GPU with the most dedicated memory first (so a discrete card
/// wins over an integrated one; amdgpu reports it, others keep their order).
#[cfg(target_os = "linux")]
fn render_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<(u64, PathBuf)> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .map(|e| {
            let vram = std::fs::read_to_string(e.path().join("device/mem_info_vram_total"))
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            (vram, PathBuf::from("/dev/dri").join(e.file_name()))
        })
        .collect();
    nodes.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    nodes.into_iter().map(|(_, p)| p).collect()
}

/// Encode a few frames of a test pattern; true if the encoder works.
fn works(enc: &HwEncoder) -> bool {
    let mut cmd = crate::render::ffmpeg();
    cmd.args(["-v", "error"]);
    enc.input_args(&mut cmd);
    cmd.format("lavfi")
        .input("color=black:s=320x240:r=30")
        .frames(5)
        .args(["-vf", &format!("null{}", enc.filter_suffix())]);
    enc.quality_args(&mut cmd);
    cmd.format("null").output("-");
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    if let Ok(events) = child.iter() {
        events.for_each(drop); // drain stderr so ffmpeg never blocks on it
    }
    child.wait().is_ok_and(|s| s.success())
}

static DETECTED: OnceLock<Option<HwEncoder>> = OnceLock::new();

/// The first working hardware encoder (probed once, ~100 ms per candidate).
pub fn detect() -> Option<&'static HwEncoder> {
    DETECTED
        .get_or_init(|| {
            let found = candidates().into_iter().find(works);
            if std::env::var_os("CLIPMAN_DEBUG").is_some() {
                eprintln!("GPU encoder: {found:?}");
            }
            found
        })
        .as_ref()
}

/// The detection result if it has finished (for the UI, which mustn't block).
pub fn detected() -> Option<Option<&'static HwEncoder>> {
    DETECTED.get().map(Option::as_ref)
}
