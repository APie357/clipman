// SPDX-License-Identifier: Apache-2.0
//! Plugin metadata on source clips, kept in a sidecar next to the clip
//! (`Replay_….mp4.clipman.json`) so the recording itself is never rewritten:
//!
//! ```json
//! { "format": 1, "plugins": { "<plugin id>": <anything> } }
//! ```
//!
//! The clip browser only lists video files, so sidecars don't show up there.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

pub const SUFFIX: &str = ".clipman.json";

pub fn sidecar_path(clip: &Path) -> PathBuf {
    let mut name = clip.file_name().unwrap_or_default().to_os_string();
    name.push(SUFFIX);
    clip.with_file_name(name)
}

/// Every plugin's data for `clip` (empty when there's no sidecar or it's unreadable).
pub fn read_all(clip: &Path) -> Map<String, Value> {
    std::fs::read(sidecar_path(clip))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|mut v| match v.get_mut("plugins").map(Value::take) {
            Some(Value::Object(m)) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// One plugin's data for `clip` (None when it has stored nothing).
pub fn read(clip: &Path, plugin: &str) -> Option<Value> {
    read_all(clip).remove(plugin)
}

/// Store (or with None / null, remove) one plugin's data, keeping the other plugins'.
/// The sidecar is removed once no plugin has data in it.
pub fn write(clip: &Path, plugin: &str, value: Option<Value>) -> std::io::Result<()> {
    let mut all = read_all(clip);
    match value {
        Some(v) if !v.is_null() => {
            all.insert(plugin.to_string(), v);
        }
        _ => {
            all.remove(plugin);
        }
    }
    let path = sidecar_path(clip);
    if all.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text = serde_json::to_string_pretty(&json!({ "format": 1, "plugins": all }))?;
    // write next to it, then rename, so a crash can't leave half a file
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, text + "\n")?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trip() {
        let dir = std::env::temp_dir().join(format!("clipman-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Replay_1.mp4");
        assert_eq!(sidecar_path(&clip), dir.join("Replay_1.mp4.clipman.json"));
        assert!(read(&clip, "a").is_none());
        write(&clip, "a", Some(json!({ "game": "x" }))).unwrap();
        write(&clip, "b", Some(json!([1, 2]))).unwrap();
        assert_eq!(read(&clip, "a"), Some(json!({ "game": "x" })));
        assert_eq!(read_all(&clip).len(), 2);
        write(&clip, "a", None).unwrap();
        assert_eq!(read_all(&clip).keys().collect::<Vec<_>>(), ["b"]);
        write(&clip, "b", Some(Value::Null)).unwrap();
        assert!(!sidecar_path(&clip).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
