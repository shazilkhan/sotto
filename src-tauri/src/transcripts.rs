//! Opt-in local transcript log, for tuning cleanup against real dictations
//! (phase 0 of docs/AI-CLEANUP-PLAN.md). One JSON line per dictation in
//! %LOCALAPPDATA%\Sotto\transcripts.jsonl. Off by default, never uploaded,
//! and the same format `--bench-cleanup` reads.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub at: u64,
    pub app: Option<String>,
    pub audio_secs: f32,
    pub asr_ms: u64,
    pub raw: String,
    /// Present only when cleanup changed the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned: Option<String>,
}

pub fn path() -> PathBuf {
    crate::settings::data_dir().join("transcripts.jsonl")
}

pub fn append(entry: &Entry) {
    let line = match serde_json::to_string(entry) {
        Ok(l) => l,
        Err(e) => return log::warn!("transcript log: {e}"),
    };
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path());
    if let Err(e) = file.and_then(|mut f| writeln!(f, "{line}")) {
        log::warn!("transcript log: {e}");
    }
}

pub fn count() -> usize {
    std::fs::read_to_string(path()).map(|s| s.lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0)
}

pub fn clear() -> std::io::Result<()> {
    match std::fs::remove_file(path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
