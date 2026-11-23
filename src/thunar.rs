// SPDX-License-Identifier: Apache-2.0
//! Thunar right-click entries ("custom actions" in ~/.config/Thunar/uca.xml), generated
//! from the presets: one entry per compress preset, plus "Open in Clipman…".

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::plugin::ClipAction;
use crate::settings::Settings;

const SUBMENU: &str = "Clipman";

pub fn uca_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("Thunar")
        .join("uca.xml")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Quote a path for Thunar's command line (it's run through a shell-like parser).
fn quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

fn action(uid: &str, icon: &str, name: &str, command: &str, desc: &str, range: &str) -> String {
    format!(
        "<action>\n\t<icon>{icon}</icon>\n\t<name>{}</name>\n\t<submenu>{SUBMENU}</submenu>\n\
         \t<unique-id>{uid}</unique-id>\n\t<command>{}</command>\n\t<description>{}</description>\n\
         \t<range>{range}</range>\n\t<patterns>*.mp4;*.MP4;*.mkv;*.MKV;*.mov;*.MOV</patterns>\n\
         \t<video-files/>\n</action>",
        esc(name),
        esc(command),
        esc(desc)
    )
}

pub fn actions_xml(s: &Settings, exe: &Path, plugin_actions: &[ClipAction]) -> String {
    let exe = quote(exe);
    let mut out: Vec<String> = s
        .presets
        .iter()
        .filter(|p| !p.is_original())
        .map(|p| {
            action(
                &format!("clipman-preset-{}", p.id),
                "video-x-generic",
                &p.menu_text(),
                &format!("{exe} --preset {} %F", p.id),
                "Render with this preset",
                "",
            )
        })
        .collect();
    // still "clipman-…", so is_ours covers them
    for a in plugin_actions.iter().filter(|a| a.file_manager) {
        out.push(action(
            &format!("clipman-plugin-{}-{}", a.plugin_id, a.id),
            "system-run",
            &a.label,
            &format!("{exe} --plugin-action {} %F", a.arg()),
            &format!("Plugin: {}", a.plugin_id),
            "",
        ));
    }
    out.push(action(
        "clipman-edit",
        "edit-cut",
        "Open in Clipman…",
        &format!("{exe} %f"),
        "Trim the clip, then save it with any preset",
        "1",
    ));
    out.join("\n")
}

/// Is this one of our actions?
fn is_ours(action: &str) -> bool {
    action.contains("<unique-id>clipman-")
}

/// Replace our entries in uca.xml with ones generated from the presets. Other custom
/// actions are kept; the previous file is saved as uca.xml.bak.
pub fn write_actions(s: &Settings, exe: &Path, plugin_actions: &[ClipAction]) -> Result<PathBuf> {
    replace_ours(&actions_xml(s, exe, plugin_actions))
}

/// Remove our entries from uca.xml (if it exists), keeping everything else.
pub fn remove_actions() -> Result<PathBuf> {
    if !uca_path().exists() {
        return Ok(uca_path());
    }
    replace_ours("")
}

fn replace_ours(actions: &str) -> Result<PathBuf> {
    let uca = uca_path();
    let xml = match std::fs::read_to_string(&uca) {
        Ok(x) => {
            std::fs::copy(&uca, uca.with_extension("xml.bak"))?;
            x
        }
        Err(_) => "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n".into(),
    };
    let Some(end) = xml.rfind("</actions>") else {
        bail!("{} doesn't look like a Thunar actions file", uca.display());
    };
    // drop our old actions, keep everything else as-is
    let mut kept = String::new();
    let mut rest = &xml[..end];
    while let Some(a) = rest.find("<action>") {
        kept.push_str(&rest[..a]);
        let Some(b) = rest[a..].find("</action>") else {
            break;
        };
        let block = &rest[a..a + b + "</action>".len()];
        if !is_ours(block) {
            kept.push_str(block);
        }
        rest = rest[a + block.len()..].trim_start_matches(['\n', '\r', ' ', '\t']);
        if !is_ours(block) {
            kept.push('\n');
        }
    }
    kept.push_str(rest);
    let new = if actions.is_empty() {
        format!("{kept}{}", &xml[end..])
    } else {
        format!("{kept}{actions}\n{}", &xml[end..])
    };
    std::fs::create_dir_all(uca.parent().unwrap())?;
    let tmp = uca.with_extension("xml.tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(tmp, &uca)?;
    Ok(uca)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_replaces() {
        let s = Settings::default();
        let plugin = |id: &str, fm| ClipAction {
            plugin_id: "up".into(),
            id: id.into(),
            label: format!("Upload {id}"),
            file_manager: fm,
        };
        let xml = actions_xml(
            &s,
            Path::new("/usr/bin/clip man"),
            &[plugin("a", true), plugin("b", false)],
        );
        assert!(xml.contains("<unique-id>clipman-plugin-up-a</unique-id>"));
        assert!(xml.contains("<command>'/usr/bin/clip man' --plugin-action up:a %F</command>"));
        assert!(!xml.contains("Upload b"));
        assert!(is_ours(&xml[xml.find("<action>").unwrap()..]));
        assert!(xml.contains("<name>720p60 · 20 MB</name>"));
        assert!(xml.contains("<command>'/usr/bin/clip man' --preset secondary %F</command>"));
        assert!(!xml.contains("--preset original"));
        assert!(is_ours(
            "<action><unique-id>clipman-edit</unique-id></action>"
        ));
        assert!(!is_ours("<action><unique-id>other-1</unique-id></action>"));
    }
}
