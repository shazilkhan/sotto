//! Weekly numbers for the Home screen: words dictated, speaking pace, time
//! saved. Stores per-day counts only, never text.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Time::SystemTimeToFileTime;

/// Typing speed the "time saved" figure compares against.
const TYPING_WPM: f64 = 40.0;
const KEEP_DAYS: u32 = 60;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Day {
    pub words: u64,
    pub secs: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    /// Local calendar day number (days since 1601-01-01) → totals.
    days: BTreeMap<u32, Day>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Week {
    pub words: u64,
    pub wpm: u64,
    pub saved_mins: u64,
}

fn path() -> PathBuf {
    crate::settings::data_dir().join("stats.json")
}

fn today() -> u32 {
    unsafe {
        let st: SYSTEMTIME = GetLocalTime();
        let mut ft = FILETIME::default();
        if SystemTimeToFileTime(&st, &mut ft).is_err() {
            return 0;
        }
        let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        (ticks / 10_000_000 / 86_400) as u32
    }
}

fn load() -> File {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn record(words: u64, secs: f32) {
    let mut f = load();
    let day = today();
    let entry = f.days.entry(day).or_default();
    entry.words += words;
    entry.secs += secs as f64;
    entry.count += 1;
    f.days.retain(|d, _| *d + KEEP_DAYS > day);
    if let Ok(json) = serde_json::to_string(&f) {
        let _ = std::fs::write(path(), json);
    }
}

/// Adds another stats file's days to this one (data moved back from elsewhere).
pub fn merge_from(other: &std::path::Path) {
    let Some(theirs) = std::fs::read_to_string(other).ok().and_then(|s| serde_json::from_str::<File>(&s).ok()) else {
        return;
    };
    let mut f = load();
    for (day, d) in theirs.days {
        let e = f.days.entry(day).or_default();
        e.words += d.words;
        e.secs += d.secs;
        e.count += d.count;
    }
    if let Ok(json) = serde_json::to_string(&f) {
        let _ = std::fs::write(path(), json);
    }
}

pub fn week() -> Week {
    summarize(&load(), today())
}

fn summarize(f: &File, today: u32) -> Week {
    let (words, secs) = f
        .days
        .iter()
        .filter(|(d, _)| **d + 7 > today)
        .fold((0u64, 0f64), |(w, s), (_, day)| (w + day.words, s + day.secs));
    let mins = secs / 60.0;
    let wpm = if mins > 0.0 { (words as f64 / mins).round() as u64 } else { 0 };
    let saved = (words as f64 / TYPING_WPM - mins).max(0.0).round() as u64;
    Week { words, wpm, saved_mins: saved }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn week_sums_last_seven_days_only() {
        let mut f = File::default();
        f.days.insert(100, Day { words: 1000, secs: 600.0, count: 10 });
        f.days.insert(94, Day { words: 140, secs: 60.0, count: 1 });
        f.days.insert(93, Day { words: 9999, secs: 1.0, count: 1 });
        let w = summarize(&f, 100);
        assert_eq!(w.words, 1140);
        assert_eq!(w.wpm, 104); // 1140 words over 11 minutes
        assert_eq!(w.saved_mins, 18); // 28.5 min typing minus 11 min talking
    }

    #[test]
    fn empty_week_is_zero() {
        let w = summarize(&File::default(), 5);
        assert_eq!((w.words, w.wpm, w.saved_mins), (0, 0, 0));
    }
}
