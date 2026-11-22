// SPDX-License-Identifier: Apache-2.0
//! The file manager's right-click menu: Thunar on Linux, Explorer on Windows.

use std::path::Path;

use anyhow::Result;

use crate::settings::Settings;

/// Whether this platform has a right-click menu clipman can manage.
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

/// Replace clipman's entries with ones generated from the presets. Returns where they
/// were written.
pub fn write(s: &Settings, exe: &Path) -> Result<String> {
    #[cfg(target_os = "linux")]
    return crate::thunar::write_actions(s, exe).map(|p| p.display().to_string());
    #[cfg(windows)]
    return crate::explorer::write_actions(s, exe);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (s, exe);
        anyhow::bail!("no right-click menu support on this platform")
    }
}

/// Remove clipman's entries (other entries are left alone).
pub fn remove() -> Result<String> {
    #[cfg(target_os = "linux")]
    return crate::thunar::remove_actions().map(|p| p.display().to_string());
    #[cfg(windows)]
    return crate::explorer::remove_actions();
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("no right-click menu support on this platform")
}
