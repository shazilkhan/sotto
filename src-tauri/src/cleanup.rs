//! Transcript cleanup between speech recognition and paste
//! (docs/AI-CLEANUP-PLAN.md). "Light" is deterministic rules that run in
//! microseconds: filler words, stutters, spoken line breaks, the user's
//! dictionary and per-app punctuation. Self-corrections and lists are left
//! for the AI tier.

use crate::dictionary::Entry;
use regex::{Captures, Regex, RegexBuilder};
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Instant;

pub struct Context {
    /// Lowercased exe of the app being dictated into, e.g. "slack.exe".
    pub app: Option<String>,
    pub light: bool,
    pub dictionary: Vec<Entry>,
}

/// Hesitation sounds, including stretched spellings ("uhhh", "ummm", "aah",
/// "ooh"). Plain "oh" and "mhm" carry meaning and are kept.
fn filler_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:u+h+m*|u+m+|e+r+m*|e+h+|a+h+|a{2,}h*|o{2,}h*|o+h{2,}|h+m+|m{2,})$").expect("valid regex")
    })
}
/// After a removed filler, a comma before one of these is kept
/// ("went well, um, and" keeps its comma; "should, uh, ship" loses it).
const CONJUNCTIONS: &[&str] = &["and", "but", "or", "so", "because", "then", "which", "while"];
/// Doubled words that are often correct ("I know that that works").
const REAL_DOUBLES: &[&str] = &["that", "had"];
const TERMINALS: &[&str] = &[
    "windowsterminal.exe", "cmd.exe", "powershell.exe", "pwsh.exe", "wezterm-gui.exe",
    "alacritty.exe", "conhost.exe", "mintty.exe", "warp.exe", "tabby.exe", "hyper.exe",
];
const CHATS: &[&str] = &[
    "slack.exe", "discord.exe", "whatsapp.exe", "telegram.exe", "ms-teams.exe", "teams.exe",
    "signal.exe", "messenger.exe",
];

pub fn clean(raw: &str, ctx: &Context) -> String {
    let text = raw.trim();
    if !ctx.light || text.is_empty() {
        return text.to_string();
    }
    let text = spoken_breaks(text);
    let text = text.lines().map(|l| collapse_repeats(&remove_fillers(l))).collect::<Vec<_>>().join("\n");
    let text = apply_dictionary(&text, &ctx.dictionary);
    fit_app(text.trim(), ctx.app.as_deref())
}

fn core(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric())
}

fn trailing_punct(word: &str) -> &str {
    let end = word.trim_end_matches(|c: char| !c.is_alphanumeric());
    &word[end.len()..]
}

fn ends_sentence(word: &str) -> bool {
    word.ends_with(['.', '!', '?'])
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn lowercase_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn is_filler(word: &str) -> bool {
    let c = core(word);
    // "ER", "UM" written in capitals are acronyms, not hesitations.
    if c.len() > 1 && c.chars().all(|ch| ch.is_ascii_uppercase()) {
        return false;
    }
    filler_regex().is_match(&c.to_lowercase())
}

/// Drops um/uh/hmm and fixes the punctuation around them.
fn remove_fillers(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut capitalize_next = false;
    for (i, w) in words.iter().enumerate() {
        if !is_filler(w) {
            out.push(if capitalize_next { capitalize(w) } else { w.to_string() });
            capitalize_next = false;
            continue;
        }
        let punct = trailing_punct(w);
        let at_sentence_start = out.last().is_none_or(|p| ends_sentence(p));
        if at_sentence_start {
            // "Um, so I think" -> "So I think". Only re-capitalise if the
            // filler itself was capitalised (it started the sentence).
            capitalize_next = capitalize_next || w.starts_with(char::is_uppercase);
            continue;
        }
        let prev = out.last_mut().expect("not at sentence start");
        if punct.contains(['.', '!', '?']) {
            // "ship it, uh." -> "ship it."
            let trimmed = prev.trim_end_matches(',').to_string();
            *prev = trimmed + &punct.replace(',', "");
            continue;
        }
        if prev.ends_with(',') {
            let next = words.get(i + 1);
            if next.is_some_and(|n| is_filler(n)) {
                continue; // decide at the last filler in a run
            }
            let next = next.map(|n| core(n).to_lowercase()).unwrap_or_default();
            if !CONJUNCTIONS.contains(&next.as_str()) {
                prev.pop();
            }
        }
    }
    out.join(" ")
}

/// "I I think the the build" -> "I think the build".
fn collapse_repeats(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for w in line.split_whitespace() {
        if let Some(prev) = out.last_mut() {
            let (a, b) = (core(prev).to_lowercase(), core(w).to_lowercase());
            let prev_bare = trailing_punct(prev).is_empty() && !prev.starts_with(|c: char| !c.is_alphanumeric());
            if prev_bare && a == b && a.chars().all(char::is_alphabetic) && !REAL_DOUBLES.contains(&a.as_str()) {
                prev.push_str(trailing_punct(w));
                continue;
            }
        }
        out.push(w.to_string());
    }
    out.join(" ")
}

fn breaks_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(r"(?:^|(?P<end>[.!?])|[,;:])\s*new\s+(?P<kind>paragraph|line)\s*(?:[.,;:!?]+|$)\s*")
            .case_insensitive(true)
            .build()
            .expect("valid regex")
    })
}

/// "Milk, new line, eggs" -> "Milk\nEggs". Only when the phrase stands on
/// its own between punctuation, so "add a new line here" is left alone.
fn spoken_breaks(text: &str) -> String {
    let replaced = breaks_regex().replace_all(text, |c: &Captures| {
        let end = c.name("end").map(|m| m.as_str()).unwrap_or("");
        let brk = if c["kind"].eq_ignore_ascii_case("paragraph") { "\n\n" } else { "\n" };
        format!("{end}{brk}")
    });
    if !replaced.contains('\n') {
        return replaced.into_owned();
    }
    let lines: Vec<String> = replaced.trim().split('\n').map(|l| capitalize(l.trim())).collect();
    // A dictated list of short items ("new line" only, never paragraphs)
    // loses the full stop on each item.
    let short = !lines.iter().any(|l| l.is_empty())
        && lines.iter().all(|l| l.split_whitespace().count() <= 3);
    lines
        .into_iter()
        .map(|l| if short { l.trim_end_matches('.').to_string() } else { l })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Swaps each "heard as" spelling (and any other casing of the word) for the
/// user's spelling, on whole words only.
fn apply_dictionary(text: &str, dictionary: &[Entry]) -> String {
    let mut pairs: Vec<(&str, &str)> = dictionary
        .iter()
        .flat_map(|e| e.aliases.iter().map(|a| (a.as_str(), e.word.as_str())).chain([(e.word.as_str(), e.word.as_str())]))
        .collect();
    pairs.sort_by_key(|(alias, _)| std::cmp::Reverse(alias.len()));
    let mut out = text.to_string();
    for (alias, word) in pairs {
        let pattern = format!(r"\b{}\b", regex::escape(alias));
        if let Ok(re) = RegexBuilder::new(&pattern).case_insensitive(true).build() {
            out = re.replace_all(&out, regex::NoExpand(word)).into_owned();
        }
    }
    out
}

/// Terminals get bare commands; chat apps drop the full stop on one-liners.
fn fit_app(text: &str, app: Option<&str>) -> String {
    let Some(app) = app else { return text.to_string() };
    if text.contains('\n') {
        return text.to_string();
    }
    if TERMINALS.contains(&app) && text.split_whitespace().count() <= 8 {
        return lowercase_first(text.strip_suffix('.').unwrap_or(text));
    }
    if CHATS.contains(&app) && !text.ends_with("..") {
        if let Some(body) = text.strip_suffix('.') {
            if !body.contains(['.', '!', '?']) {
                return body.to_string();
            }
        }
    }
    text.to_string()
}

/// One bench case. Reads both hand-written eval cases (with `expected`) and
/// lines from the transcript log (just `raw` and `app`).
#[derive(Deserialize)]
struct Case {
    raw: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    expected: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// word -> heard-as spellings
    #[serde(default)]
    dictionary: std::collections::BTreeMap<String, Vec<String>>,
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

/// `sotto --bench-cleanup cases.jsonl`: runs every case through Light
/// cleanup with the user's saved dictionary plus any in the case, and prints
/// failures, changes and timing.
pub fn bench(path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return println!("can't read {path}: {e}"),
    };
    let saved = crate::dictionary::load();
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
        let mut dictionary = saved.clone();
        dictionary.extend(case.dictionary.iter().map(|(w, a)| Entry { word: w.clone(), aliases: a.clone(), learned: false }));
        let ctx = Context { app: case.app.clone(), light: true, dictionary };
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

    fn light(raw: &str) -> String {
        clean(raw, &Context { app: None, light: true, dictionary: vec![] })
    }

    #[test]
    fn off_is_pass_through() {
        let ctx = Context { app: None, light: false, dictionary: vec![] };
        assert_eq!(clean("  Um, hello.  ", &ctx), "Um, hello.");
    }

    #[test]
    fn fillers_and_their_commas_go() {
        assert_eq!(light("Um, so I think we should, uh, ship it on Friday."), "So I think we should ship it on Friday.");
        assert_eq!(light("The demo went well, um, and the client wants a call."), "The demo went well, and the client wants a call.");
        assert_eq!(light("Ship it, uh."), "Ship it.");
        assert_eq!(light("Uh uh uh, okay, make the repo public."), "Okay, make the repo public.");
        assert_eq!(light("Um."), "");
    }

    #[test]
    fn stretched_and_other_sounds_go() {
        assert_eq!(light("Ahh, that makes sense."), "That makes sense.");
        assert_eq!(light("Ooh, nice one. Uhhh, ship it."), "Nice one. Ship it.");
        assert_eq!(light("So, ummm, hmmm, let's go."), "So let's go.");
        assert_eq!(light("Eh, fine."), "Fine.");
    }

    #[test]
    fn meaningful_sounds_and_acronyms_stay() {
        assert_eq!(light("Oh, I see."), "Oh, I see.");
        assert_eq!(light("Mhm, sounds right."), "Mhm, sounds right.");
        assert_eq!(light("We went to the ER last night."), "We went to the ER last night.");
        assert_eq!(light("Ahead of time, ahem."), "Ahead of time, ahem.");
    }

    #[test]
    fn fillers_inside_words_stay() {
        assert_eq!(light("The umbrella and the hummus."), "The umbrella and the hummus.");
        assert_eq!(light("Uh-huh, that works."), "Uh-huh, that works.");
    }

    #[test]
    fn stutters_collapse_but_real_doubles_stay() {
        assert_eq!(light("I I think the the build is broken."), "I think the build is broken.");
        assert_eq!(light("I know that that works."), "I know that that works.");
        assert_eq!(light("Version 10 10 is out."), "Version 10 10 is out.");
    }

    #[test]
    fn spoken_breaks_only_when_standalone() {
        assert_eq!(light("Thanks. New paragraph. I'll send it."), "Thanks.\n\nI'll send it.");
        assert_eq!(light("Milk, new line, eggs, new line, bread."), "Milk\nEggs\nBread");
        assert_eq!(light("Add a new line to the file."), "Add a new line to the file.");
    }

    #[test]
    fn dictionary_swaps_whole_words() {
        let ctx = Context {
            app: None,
            light: true,
            dictionary: vec![Entry { word: "Supabase".into(), aliases: vec!["supa base".into()], learned: false }],
        };
        assert_eq!(clean("Pushed to supa base and supabase.", &ctx), "Pushed to Supabase and Supabase.");
        assert_eq!(clean("Supabases are nice.", &ctx), "Supabases are nice.");
    }

    #[test]
    fn app_context() {
        let term = Context { app: Some("windowsterminal.exe".into()), light: true, dictionary: vec![] };
        assert_eq!(clean("Git status.", &term), "git status");
        let chat = Context { app: Some("slack.exe".into()), light: true, dictionary: vec![] };
        assert_eq!(clean("Sounds good, see you then.", &chat), "Sounds good, see you then");
        assert_eq!(clean("Done. See you then.", &chat), "Done. See you then.");
        assert_eq!(clean("Really?", &chat), "Really?");
    }

    #[test]
    fn normalize_ignores_spacing_but_keeps_lines() {
        assert_eq!(normalize("  a   b \n  c  "), "a b\nc");
        assert_ne!(normalize("a b"), normalize("a\nb"));
    }
}
