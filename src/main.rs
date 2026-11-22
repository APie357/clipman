// SPDX-License-Identifier: Apache-2.0
//! clipman: browse, trim and compress game clips (e.g. to fit Discord's upload limit).
//!
//!   clipman                          open the clip browser
//!   clipman CLIP                     open CLIP in the editor
//!   clipman CLIP... --preset ID ...  render without the editor (progress in the
//!                                    terminal, or in a small window when started
//!                                    from a file manager)

// No console window for the GUI on Windows (the CLI re-attaches to its parent's).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, test))]
mod explorer;
mod gpu;
mod media;
mod menu;
mod player;
mod render;
mod settings;
#[cfg(any(target_os = "linux", test))]
mod thunar;
mod ui;
mod update;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use clap::Parser;

use settings::Settings;

#[derive(Parser, Debug)]
#[command(
    name = "clipman",
    version,
    about = "Browse, trim and compress game clips"
)]
struct Cli {
    /// Clip(s). One clip without render options opens the editor; none opens the browser.
    files: Vec<PathBuf>,

    /// Render with this preset (id from settings; default: the first compress preset).
    #[arg(long)]
    preset: Option<String>,
    /// Render: start of the range to keep, in seconds.
    #[arg(long)]
    start: Option<f64>,
    /// Render: end of the range to keep, in seconds.
    #[arg(long)]
    end: Option<f64>,
    /// Render: audio tracks to mix into one, 1-based (e.g. 1,2), or "none".
    /// Default: tracks ticked by default in Settings.
    #[arg(long)]
    audio: Option<String>,
    /// Render: tracks to run noise suppression on, 1-based (e.g. 2,3), or "none".
    /// Default: tracks set to "On by default" in Settings.
    #[arg(long)]
    denoise: Option<String>,
    /// Render: output file (only with a single clip). Default: <prefix><name><suffix>.mp4.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Render with the defaults (same as giving any render option).
    #[arg(long)]
    render: bool,
    /// Render: video encoder (auto: GPU only for presets without a size limit).
    /// Default: the one chosen in Settings.
    #[arg(long, value_enum)]
    encoder: Option<settings::Encoder>,
    /// Render: show progress in the terminal even when not attached to one.
    #[arg(long)]
    no_window: bool,

    /// Open only the Settings window.
    #[arg(long)]
    settings: bool,
    /// Regenerate the right-click menu entries from the presets and exit (Thunar on
    /// Linux, Explorer on Windows).
    #[arg(long, alias = "write-thunar")]
    write_menu: bool,
    /// Remove clipman's right-click menu entries and exit.
    #[arg(long)]
    remove_menu: bool,
    /// Download / verify the noise suppression model, print its path and exit.
    #[arg(long)]
    fetch_model: bool,
}

impl Cli {
    fn wants_render(&self) -> bool {
        self.render
            || self.preset.is_some()
            || self.start.is_some()
            || self.end.is_some()
            || self.audio.is_some()
            || self.denoise.is_some()
            || self.out.is_some()
            || self.encoder.is_some()
            || self.files.len() > 1
    }
}

fn parse_tracks(s: &str) -> Result<Vec<usize>, String> {
    if s.eq_ignore_ascii_case("none") {
        return Ok(vec![]);
    }
    s.split(',')
        .map(|t| match t.trim().parse::<usize>() {
            Ok(n) if n >= 1 => Ok(n - 1),
            _ => Err(format!(
                "bad track number {t:?} (tracks are numbered from 1)"
            )),
        })
        .collect()
}

fn build_jobs(cli: &Cli, settings: &Settings) -> Result<Vec<render::Job>, String> {
    let preset = match &cli.preset {
        Some(id) => settings.preset(id).ok_or_else(|| {
            format!("No such preset: {id}. Check the presets in Settings (and regenerate the right-click menu).")
        })?,
        None => settings
            .first_compress_preset()
            .or(settings.presets.first())
            .ok_or("There are no presets. Add one in Settings.")?,
    };
    if cli.out.is_some() && cli.files.len() > 1 {
        return Err("--out only works with a single clip".into());
    }
    let audio = cli.audio.as_deref().map(parse_tracks).transpose()?;
    let denoise = cli.denoise.as_deref().map(parse_tracks).transpose()?;
    let mut jobs = Vec::new();
    for src in &cli.files {
        if !src.is_file() {
            return Err(format!("{} isn't a file", src.display()));
        }
        let names = media::probe(src)
            .map(|i| i.audio_names)
            .map_err(|e| format!("{}: {e:#}", src.display()))?;
        let (d_tracks, d_denoise) = settings.default_audio(&names);
        jobs.push(render::Job {
            src: src.clone(),
            dst: cli
                .out
                .clone()
                .unwrap_or_else(|| settings.output_path(src, &preset.suffix)),
            preset: preset.clone(),
            start: cli.start,
            end: cli.end,
            tracks: audio.clone().unwrap_or(d_tracks),
            denoise: denoise.clone().unwrap_or(d_denoise),
            strength: settings.denoise_strength,
            encoder: cli.encoder.unwrap_or(settings.encoder),
        });
    }
    Ok(jobs)
}

/// Render in the terminal with a one-line progress display.
fn render_in_terminal(jobs: &[render::Job]) -> ExitCode {
    let cancel = AtomicBool::new(false);
    let mut failed = false;
    for (i, job) in jobs.iter().enumerate() {
        let name = job
            .src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let prefix = if jobs.len() > 1 {
            format!("[{}/{}] ", i + 1, jobs.len())
        } else {
            String::new()
        };
        let mut last = String::new();
        let mut report = |f: f32, label: &str| {
            let line = format!("\r{prefix}{name}: {label} {:3.0}%", f * 100.0);
            if line != last {
                eprint!("{line}\x1b[K");
                let _ = std::io::stderr().flush();
                last = line;
            }
        };
        match render::render(job, &mut report, &cancel) {
            Ok((p, size)) => eprintln!(
                "\r{prefix}{} ({:.2} MB)\x1b[K",
                p.display(),
                size as f64 / 1e6
            ),
            Err(e) => {
                eprintln!("\r{prefix}{name}: {e:#}\x1b[K");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Show an error without a terminal (e.g. started from the file manager).
fn error_dialog(msg: &str) {
    eprintln!("clipman: {msg}");
    if !std::io::stderr().is_terminal() {
        rfd::MessageDialog::new()
            .set_title("Clipman")
            .set_description(msg)
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

#[cfg(windows)]
fn attach_console() {
    // With windows_subsystem = "windows" there's no console; reuse the parent's (if
    // started from a terminal) so CLI output is visible.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

fn print_or_fail<T: std::fmt::Display>(r: anyhow::Result<T>) -> ExitCode {
    match r {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("clipman: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    #[cfg(windows)]
    attach_console();
    let mut cli = Cli::parse();
    // ffmpeg runs from the cache dir (for the noise model), so paths must be absolute
    for f in &mut cli.files {
        *f = std::path::absolute(&*f).unwrap_or_else(|_| f.clone());
    }
    cli.out = cli.out.map(|o| std::path::absolute(&o).unwrap_or(o));

    if cli.fetch_model {
        return print_or_fail(render::ensure_model().map(|p| p.display().to_string()));
    }
    if cli.write_menu {
        let res = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| menu::write(&Settings::load(), &exe));
        return print_or_fail(res);
    }
    if cli.remove_menu {
        return print_or_fail(menu::remove());
    }
    if let Err(e) = media::ensure_ffmpeg() {
        error_dialog(&format!("{e:#}. Install ffmpeg and try again."));
        return ExitCode::FAILURE;
    }

    let start = if cli.settings {
        ui::Start::Settings
    } else if cli.wants_render() {
        let jobs = match build_jobs(&cli, &Settings::load()) {
            Ok(j) if !j.is_empty() => j,
            Ok(_) => {
                error_dialog("No clips given.");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                error_dialog(&e);
                return ExitCode::FAILURE;
            }
        };
        if cli.no_window || std::io::stderr().is_terminal() {
            return render_in_terminal(&jobs);
        }
        ui::Start::Render(jobs)
    } else if let Some(f) = cli.files.first() {
        if !f.is_file() {
            error_dialog(&format!("{} isn't a file", f.display()));
            return ExitCode::FAILURE;
        }
        ui::Start::Editor(f.clone())
    } else {
        ui::Start::Browser
    };
    match ui::run(start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error_dialog(&format!("Couldn't open a window: {e}"));
            ExitCode::FAILURE
        }
    }
}
