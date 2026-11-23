// SPDX-License-Identifier: Apache-2.0
//! Windows Explorer right-click entries, generated from the presets like the Thunar
//! ones: a "Clipman" submenu on video files with one entry per compress preset, plus
//! "Open in Clipman…". Per-user (HKCU, no admin rights needed), and only ever touches
//! the `clipman` key under each extension's `shell`. On Windows 11 they're under
//! "Show more options".

use std::path::Path;

use crate::plugin::ClipAction;
use crate::settings::Settings;

/// File types that get the menu (Explorer matches extensions case-insensitively).
pub const EXTENSIONS: [&str; 3] = [".mp4", ".mkv", ".mov"];

/// Our key under HKCU for one extension. Everything clipman writes lives below it.
pub fn root_key(ext: &str) -> String {
    format!(r"Software\Classes\SystemFileAssociations\{ext}\shell\clipman")
}

/// A registry key (relative to HKCU) and its string values ("" = the default value).
#[derive(Debug, PartialEq)]
pub struct Key {
    pub path: String,
    pub values: Vec<(&'static str, String)>,
}

/// Quote an argument for a Windows command line.
fn quote(p: &Path) -> String {
    format!("\"{}\"", p.to_string_lossy())
}

/// Every key to create for one extension, parents first.
pub fn keys(s: &Settings, exe: &Path, ext: &str, plugin_actions: &[ClipAction]) -> Vec<Key> {
    let root = root_key(ext);
    let exe = quote(exe);
    let mut keys = vec![Key {
        path: root.clone(),
        values: vec![("MUIVerb", "Clipman".into()), ("SubCommands", String::new())],
    }];
    // submenu entries are sorted by key name, so number them to keep the preset order
    let mut entry = |n: usize, name: String, command: String| {
        let path = format!(r"{root}\shell\{n:02}");
        keys.push(Key {
            path: path.clone(),
            values: vec![("MUIVerb", name)],
        });
        keys.push(Key {
            path: format!(r"{path}\command"),
            values: vec![("", command)],
        });
    };
    let presets: Vec<_> = s.presets.iter().filter(|p| !p.is_original()).collect();
    for (i, p) in presets.iter().enumerate() {
        entry(
            i + 1,
            p.menu_text(),
            format!("{exe} --preset {} \"%1\"", p.id),
        );
    }
    let mut n = presets.len();
    for a in plugin_actions.iter().filter(|a| a.file_manager) {
        n += 1;
        entry(
            n,
            a.label.clone(),
            format!("{exe} --plugin-action {} \"%1\"", a.arg()),
        );
    }
    entry(n + 1, "Open in Clipman…".into(), format!("{exe} \"%1\""));
    keys
}

/// Replace our entries with ones generated from the presets. Returns a description of
/// where they went.
#[cfg(windows)]
pub fn write_actions(
    s: &Settings,
    exe: &Path,
    plugin_actions: &[ClipAction],
) -> anyhow::Result<String> {
    for ext in EXTENSIONS {
        reg::delete_tree(&root_key(ext))?;
        for k in keys(s, exe, ext, plugin_actions) {
            reg::write(&k)?;
        }
    }
    reg::notify_shell();
    Ok(format!(
        r"HKEY_CURRENT_USER\Software\Classes\SystemFileAssociations\{{{}}}\shell\clipman",
        EXTENSIONS.join(",")
    ))
}

/// Remove every entry clipman added.
#[cfg(windows)]
pub fn remove_actions() -> anyhow::Result<String> {
    for ext in EXTENSIONS {
        reg::delete_tree(&root_key(ext))?;
    }
    reg::notify_shell();
    Ok("removed the Explorer right-click entries".into())
}

#[cfg(windows)]
mod reg {
    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    pub fn delete_tree(path: &str) -> Result<()> {
        let p = wide(path);
        let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, p.as_ptr()) };
        if err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND {
            bail!("couldn't remove HKCU\\{path} (error {err})");
        }
        Ok(())
    }

    pub fn write(k: &super::Key) -> Result<()> {
        let p = wide(&k.path);
        let mut key: HKEY = std::ptr::null_mut();
        let err = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                p.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if err != ERROR_SUCCESS {
            bail!("couldn't create HKCU\\{} (error {err})", k.path);
        }
        let mut result = Ok(());
        for (name, value) in &k.values {
            let n = wide(name);
            let v = wide(value);
            let err = unsafe {
                RegSetValueExW(
                    key,
                    if name.is_empty() {
                        std::ptr::null()
                    } else {
                        n.as_ptr()
                    },
                    0,
                    REG_SZ,
                    v.as_ptr().cast(),
                    (v.len() * 2) as u32,
                )
            };
            if err != ERROR_SUCCESS {
                result = Err(anyhow::anyhow!(
                    "couldn't set {name:?} in HKCU\\{} (error {err})",
                    k.path
                ));
                break;
            }
        }
        unsafe { RegCloseKey(key) };
        result
    }

    /// Tell Explorer the menus changed (otherwise it may only notice after a restart).
    pub fn notify_shell() {
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_keys() {
        let s = Settings::default();
        let keys = keys(&s, Path::new(r"C:\Program Files\clipman\clipman.exe"), ".mp4", &[]);
        let root = r"Software\Classes\SystemFileAssociations\.mp4\shell\clipman";
        assert_eq!(keys[0].path, root);
        assert!(keys[0].values.contains(&("SubCommands", String::new())));
        let find = |p: &str| keys.iter().find(|k| k.path == format!(r"{root}\{p}")).unwrap();
        assert_eq!(find(r"shell\01").values, [("MUIVerb", "720p60 · 20 MB".into())]);
        assert_eq!(
            find(r"shell\02\command").values,
            [(
                "",
                r#""C:\Program Files\clipman\clipman.exe" --preset secondary "%1""#.into()
            )]
        );
        // "Source" presets aren't offered; "Open in Clipman…" comes last
        assert_eq!(find(r"shell\03").values, [("MUIVerb", "Open in Clipman…".into())]);
        assert!(keys.iter().all(|k| k.path.starts_with(root)));
        assert_eq!(keys.len(), 1 + 3 * 2);
        for ext in EXTENSIONS {
            assert!(super::keys(&s, Path::new("c.exe"), ext, &[])[0].path.contains(ext));
        }
        let up = ClipAction {
            plugin_id: "up".into(),
            id: "send".into(),
            label: "Upload".into(),
            file_manager: true,
        };
        let keys = super::keys(&s, Path::new("c.exe"), ".mp4", &[up]);
        let find = |p: &str| keys.iter().find(|k| k.path == format!(r"{root}\{p}")).unwrap();
        assert_eq!(find(r"shell\03").values, [("MUIVerb", "Upload".into())]);
        assert_eq!(
            find(r"shell\03\command").values,
            [("", r#""c.exe" --plugin-action up:send "%1""#.into())]
        );
        assert_eq!(find(r"shell\04").values, [("MUIVerb", "Open in Clipman…".into())]);
    }
}
