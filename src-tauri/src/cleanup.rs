//! Transcript cleanup between speech recognition and paste. Phase 0 is a
//! pass-through so `--bench-cleanup` has a baseline to measure the rule and
//! AI tiers against (docs/AI-CLEANUP-PLAN.md).

use serde::Deserialize;
use std::time::Instant;

pub struct Context {
    /// Lowercased exe of the app being dictated into, e.g. "slack.exe".
    #[allow(dead_code)] // read by the rule tier (phase 1)
    pub app: Option<String>,
}

pub fn clean(raw: &str, _ctx: &Context) -> String {
    raw.trim().to_string()
}

/// One bench case. Reads both hand-written eval cases (with `expected`) and
/// lines from the transcript log (just `raw` and `app`). Unknown fields are
/// ignored, so cases can carry data for later tiers (e.g. a dictionary).
#[derive(Deserialize)]
struct Case {
    raw: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    expected: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// Whitespace-insensitive comparison that still respects line breaks.
fn normalize(s: &str) -> String {
    s.trim()
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn show(s: &str) -> String {
    s.replace('\n', "\\n")
}

/// `sotto --bench-cleanup cases.jsonl`: runs every case through `clean` and
/// prints failures, changes and timing.
pub fn bench(path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return println!("can't read {path}: {e}"),
    };
    let (mut total, mut graded, mut passed, mut changed) = (0, 0, 0, 0);
    let mut times = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let case: Case = match serde_json::from_str(line) {
            Ok(c) => c,
            Err(e) => {
                println!("line {}: not a case ({e})", i + 1);
                continue;
            }
        };
        total += 1;
        let ctx = Context { app: case.app.clone() };
        let t = Instant::now();
        let out = clean(&case.raw, &ctx);
        times.push(t.elapsed().as_micros());

        let label = format!(
            "#{} {}{}",
            i + 1,
            case.note.as_deref().unwrap_or(""),
            case.app.as_deref().map(|a| format!(" [{a}]")).unwrap_or_default()
        );
        let did_change = normalize(&out) != normalize(&case.raw);
        if did_change {
            changed += 1;
        }
        match &case.expected {
            Some(expected) => {
                graded += 1;
                if normalize(&out) == normalize(expected) {
                    passed += 1;
                } else {
                    println!("FAIL {label}");
                    println!("  raw:      {}", show(&case.raw));
                    println!("  expected: {}", show(expected));
                    println!("  got:      {}", show(&out));
                }
            }
            None if did_change => {
                println!("CHANGED {label}");
                println!("  raw: {}", show(&case.raw));
                println!("  got: {}", show(&out));
            }
            None => {}
        }
    }
    times.sort_unstable();
    let p50 = times.get(times.len() / 2).copied().unwrap_or(0);
    let max = times.last().copied().unwrap_or(0);
    println!();
    println!("{total} cases: {passed}/{graded} graded cases pass, {changed} changed by cleanup");
    println!("time per case: p50 {p50} us, max {max} us");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ignores_spacing_but_keeps_lines() {
        assert_eq!(normalize("  a   b \n  c  "), "a b\nc");
        assert_ne!(normalize("a b"), normalize("a\nb"));
    }

    #[test]
    fn phase0_clean_is_pass_through() {
        let ctx = Context { app: None };
        assert_eq!(clean("  Um, hello.  ", &ctx), "Um, hello.");
    }
}
