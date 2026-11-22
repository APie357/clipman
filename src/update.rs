// SPDX-License-Identifier: Apache-2.0
//! Update checker: asks GitHub for the latest release, at most once a day, in the
//! background when a window opens. Never downloads or installs anything; the UI just
//! links to the release page. Builds without the `update-check` feature (distro
//! packages, whose package manager does updates) never check.
#![cfg_attr(not(feature = "update-check"), allow(dead_code))]

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const REPO: &str = "APie357/clipman";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
#[cfg(feature = "update-check")]
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// Version without the leading "v", e.g. "0.2.0".
    pub version: String,
    /// The release page.
    pub url: String,
}

/// What's remembered between runs (in the cache dir).
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Unix time of the last successful check.
    #[serde(default)]
    checked_at: u64,
    /// The newest release seen then.
    #[serde(default)]
    latest: Option<Release>,
    /// A version the user dismissed.
    #[serde(default)]
    skipped: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Built without the update checker.
    Disabled,
    Checking,
    UpToDate,
    Available(Release),
    Failed(String),
}

static STATUS: OnceLock<Status> = OnceLock::new();
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub const ENABLED: bool = cfg!(feature = "update-check");

fn state_path() -> std::path::PathBuf {
    crate::settings::cache_dir().join("update-check.json")
}

fn load_state() -> State {
    std::fs::read(state_path())
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

fn save_state(s: &State) {
    let path = state_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(path, serde_json::to_vec_pretty(s).unwrap_or_default());
}

/// Numeric (major, minor, patch) and whether it's a pre-release ("1.2.0-rc1").
fn parse(v: &str) -> Option<([u64; 3], bool)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((c, rest)) => (c, v.as_bytes()[c.len()] == b'-' && !rest.is_empty()),
        None => (v, false),
    };
    let mut n = [0u64; 3];
    let mut parts = core.split('.');
    for slot in &mut n {
        match parts.next() {
            Some(p) => *slot = p.parse().ok()?,
            None => break,
        }
    }
    Some((n, pre))
}

/// Is `latest` newer than `current`? Pre-releases are never offered.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some((l, false)), Some((c, c_pre))) => l > c || (l == c && c_pre),
        _ => false,
    }
}

/// Start the background check (once per process; does nothing when disabled in
/// Settings or at build time).
pub fn start(enabled_in_settings: bool) {
    let first = ENABLED
        && enabled_in_settings
        && !STARTED.swap(true, std::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "update-check")]
    if first {
        std::thread::spawn(|| {
            let _ = STATUS.set(check());
        });
    }
    #[cfg(not(feature = "update-check"))]
    let _ = first;
}

/// The result so far, for the UI.
pub fn status() -> Status {
    if !ENABLED {
        return Status::Disabled;
    }
    STATUS.get().cloned().unwrap_or(Status::Checking)
}

/// The update to offer, unless the user dismissed that version.
pub fn offer() -> Option<Release> {
    match STATUS.get()? {
        Status::Available(r) if load_state().skipped.as_deref() != Some(&r.version) => {
            Some(r.clone())
        }
        _ => None,
    }
}

/// Don't offer this version again.
pub fn skip(version: &str) {
    let mut s = load_state();
    s.skipped = Some(version.to_string());
    save_state(&s);
}

#[cfg(feature = "update-check")]
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(feature = "update-check")]
fn check() -> Status {
    let mut state = load_state();
    let fresh = now().saturating_sub(state.checked_at) < CHECK_EVERY_SECS;
    if !fresh {
        match fetch_latest() {
            Ok(r) => {
                state.checked_at = now();
                state.latest = Some(r);
                save_state(&state);
            }
            Err(e) => {
                if std::env::var_os("CLIPMAN_DEBUG").is_some() {
                    eprintln!("update check failed: {e:#}");
                }
                // fall back to what was seen last time, if anything
                if state.latest.is_none() {
                    return Status::Failed(format!("{e:#}"));
                }
            }
        }
    }
    match state.latest {
        Some(r) if is_newer(&r.version, CURRENT) => Status::Available(r),
        _ => Status::UpToDate,
    }
}

#[cfg(feature = "update-check")]
fn fetch_latest() -> anyhow::Result<Release> {
    use anyhow::Context;

    #[derive(Deserialize)]
    struct GhRelease {
        tag_name: String,
        html_url: String,
    }
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .new_agent();
    let body = agent
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", format!("clipman/{CURRENT}"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .context("couldn't reach GitHub")?;
    let gh: GhRelease = serde_json::from_str(&body).context("unexpected reply from GitHub")?;
    Ok(Release {
        version: gh.tag_name.trim_start_matches(['v', 'V']).to_string(),
        url: gh.html_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0")); // numeric, not text
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("0.2.0-rc1", "0.1.0")); // pre-releases aren't offered
        assert!(is_newer("0.2.0", "0.2.0-rc1"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("0.2.0+build5", "0.2.0"));
    }
}
