//! Favorites, last-used node and a couple of view settings, in
//! `~/.config/tailgate/state.json` (Linux; %APPDATA%\Tailgate on Windows,
//! ~/Library/Application Support/Tailgate on macOS). Node names only, no secrets.
//! Optional names for served ports come from `labels.json` beside it, which
//! Tailgate only reads.

use crate::nodes::Favorite;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub favorites: Vec<SavedFavorite>,
    pub last_used: Option<LastUsed>,
    pub all_servers: bool,
    pub dark: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFavorite {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastUsed {
    pub id: String,
    pub label: String,
}

impl Saved {
    pub fn favorites(&self) -> Vec<Favorite> {
        self.favorites.iter().map(|f| Favorite { id: f.id.clone(), label: f.label.clone() }).collect()
    }

    pub fn toggle_favorite(&mut self, id: &str, label: &str) {
        if let Some(i) = self.favorites.iter().position(|f| f.id == id) {
            self.favorites.remove(i);
        } else {
            self.favorites.push(SavedFavorite { id: id.into(), label: label.into() });
        }
    }

    /// True if this changed anything, so callers only write when they must.
    pub fn remember(&mut self, id: &str, label: &str) -> bool {
        let new = LastUsed { id: id.into(), label: label.into() };
        if self.last_used.as_ref() == Some(&new) {
            return false;
        }
        self.last_used = Some(new);
        true
    }
}

pub fn default_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("TAILGATE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("state.json");
    }
    // %APPDATA%\Tailgate and ~/Library/Application Support/Tailgate.
    if cfg!(windows) {
        if let Some(dir) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(dir).join("Tailgate").join("state.json");
        }
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home).join("Library/Application Support/Tailgate/state.json");
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("tailgate").join("state.json")
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Labels {
    ports: BTreeMap<u32, String>,
}

/// Reads `{"ports": {"8443": "Dashboard"}}`. No file, or a damaged one, means no labels.
pub fn load_labels(path: &Path) -> BTreeMap<u32, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Labels>(&text).ok())
        .map(|labels| labels.ports)
        .unwrap_or_default()
}

/// A missing or damaged file means starting fresh, never failing to open.
pub fn load(path: &Path) -> Saved {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tailgate-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("state.json")
    }

    #[test]
    fn round_trips() {
        let path = temp_path("round");
        let mut saved = Saved::default();
        saved.toggle_favorite("a", "Dallas");
        saved.remember("b", "Tirana");
        save(&path, &saved).unwrap();
        assert_eq!(load(&path), saved);
    }

    #[test]
    fn missing_or_corrupt_file_starts_fresh() {
        let path = temp_path("corrupt");
        assert_eq!(load(&path), Saved::default());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), Saved::default());
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let saved: Saved = serde_json::from_str(r#"{"dark":true,"future":1}"#).unwrap();
        assert!(saved.dark && saved.favorites.is_empty());
    }

    #[test]
    fn labels_are_optional_and_tolerant() {
        let path = temp_path("labels").with_file_name("labels.json");
        assert!(load_labels(&path).is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"ports":{"8443":"Dashboard"}}"#).unwrap();
        assert_eq!(load_labels(&path).get(&8443).map(String::as_str), Some("Dashboard"));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_labels(&path).is_empty());
    }

    #[test]
    fn toggling_and_remembering() {
        let mut saved = Saved::default();
        saved.toggle_favorite("a", "A");
        assert_eq!(saved.favorites().len(), 1);
        saved.toggle_favorite("a", "A");
        assert!(saved.favorites.is_empty());
        assert!(saved.remember("a", "A"));
        assert!(!saved.remember("a", "A"));
    }
}
