//! The user's dictionary: words Sotto should always spell their way, plus
//! what speech recognition sometimes writes instead. Stored next to
//! settings.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub word: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

fn path() -> PathBuf {
    crate::settings::config_dir().join("dictionary.json")
}

pub fn load() -> Vec<Entry> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Trims, drops empty words and aliases, and drops aliases equal to the word.
pub fn normalize(entries: Vec<Entry>) -> Vec<Entry> {
    entries
        .into_iter()
        .filter_map(|e| {
            let word = e.word.trim().to_string();
            if word.is_empty() {
                return None;
            }
            let aliases = e
                .aliases
                .iter()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case(&word))
                .collect();
            Some(Entry { word, aliases })
        })
        .collect()
}

pub fn save(entries: &[Entry]) -> std::io::Result<()> {
    std::fs::create_dir_all(crate::settings::config_dir())?;
    std::fs::write(path(), serde_json::to_string_pretty(entries).expect("dictionary serialize"))
}
