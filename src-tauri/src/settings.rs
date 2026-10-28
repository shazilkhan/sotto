use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// One of the presets in `hotkey::PRESETS`.
    pub hotkey: String,
    /// Input device name. `None` follows the Windows default mic.
    pub mic: Option<String>,
    /// Mute other apps' audio while the hotkey is held, so speakers don't
    /// bleed into the mic.
    pub mute_while_dictating: bool,
    pub sounds: bool,
    pub trailing_space: bool,
    /// Minutes to keep the model in memory after the last dictation.
    /// 0 unloads right away, -1 keeps it loaded.
    pub keep_model_mins: i32,
    pub start_with_windows: bool,
    /// Append each dictation to a local JSONL file for tuning cleanup.
    pub keep_transcripts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "ctrl+win".into(),
            mic: None,
            mute_while_dictating: true,
            sounds: true,
            trailing_space: true,
            keep_model_mins: 15,
            start_with_windows: true,
            keep_transcripts: false,
        }
    }
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Sotto")
}

/// Roaming: settings follow the user.
pub fn config_dir() -> PathBuf {
    env_dir("APPDATA")
}

/// Local: models, logs and crash markers stay on this machine.
pub fn data_dir() -> PathBuf {
    env_dir("LOCALAPPDATA")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn is_first_run() -> bool {
    !settings_path().exists()
}

pub fn load() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    let json = serde_json::to_string_pretty(settings).expect("settings serialize");
    std::fs::write(settings_path(), json)
}
