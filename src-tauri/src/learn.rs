//! Learns spellings from the user's own fixes. Right after a paste, the
//! focused text field is remembered. At the next dictation (or after 25 s)
//! it's read once more, and a name the user retyped in place ("Neve" ->
//! "Niamh") becomes a Dictionary entry. A chat box empties the moment a
//! message is sent, so in the meantime the field is also read at each
//! Enter (before the app gets it) and twice a second, keeping the latest
//! reading that held still.
//!
//! Privacy: only the field that was pasted into is read, only for those
//! 25 s, never a password field, and nothing is kept except the learned
//! word pair. UI
//! Automation calls run on their own thread, so a slow or hung app can't
//! stall dictation.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationValuePattern,
    UIA_TextPatternId, UIA_ValuePatternId,
};

/// How long after a paste to look for a fix, if no new dictation comes first.
const CHECK_AFTER: Duration = Duration::from_secs(25);
/// The longest Enter is held back while the field is read.
const ENTER_WAIT: Duration = Duration::from_millis(150);
/// How often the field is read while a paste is watched, to catch the fix
/// before a Send button clears it.
const POLL: Duration = Duration::from_millis(500);
const MAX_FIELD_CHARS: i32 = 20_000;
const MAX_LEARN_PER_PASTE: usize = 2;

type OnLearn = Box<dyn Fn(String, String) + Send + Sync>;
static ON_LEARN: OnceLock<OnLearn> = OnceLock::new();

enum Cmd {
    /// Check the paste now (a new dictation started, or a newer paste).
    Check,
    /// Read the field before Enter reaches the app, then reply.
    Snapshot(Sender<()>),
}

/// Talks to the watcher for the latest paste.
static ACTIVE: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
/// A paste is being watched; lets the keyboard hook skip everything else.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// `on_learn(heard, word)` is called for each spelling the user fixed.
pub fn start(on_learn: OnLearn) {
    let _ = ON_LEARN.set(on_learn);
}

/// Checks the last paste now (called when a new dictation starts).
pub fn check_now() {
    WATCHING.store(false, Ordering::Relaxed);
    if let Some(tx) = ACTIVE.lock().unwrap().take() {
        let _ = tx.send(Cmd::Check);
    }
}

/// Called from the keyboard hook when Enter goes down, before the app gets
/// it. A chat box empties as soon as Enter sends the message, so the field
/// is read now, while the user's fix is still in it.
pub fn before_enter() {
    if !WATCHING.load(Ordering::Relaxed) {
        return;
    }
    let Some(tx) = ACTIVE.try_lock().ok().and_then(|g| g.clone()) else { return };
    let (done, wait) = channel();
    if tx.send(Cmd::Snapshot(done)).is_ok() {
        let _ = wait.recv_timeout(ENTER_WAIT);
    }
}

/// Remembers the focused field for this paste. The accessibility client
/// lives only on this short-lived thread: creating it starts a pool of
/// Windows worker threads, so it's never kept around while idle.
pub fn watch(pasted: &str) {
    check_now();
    let (tx, rx) = channel::<Cmd>();
    *ACTIVE.lock().unwrap() = Some(tx);
    WATCHING.store(true, Ordering::Relaxed);
    let pasted = pasted.trim().to_string();
    let spawned = std::thread::Builder::new().name("learn".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        {
            let automation: windows::core::Result<IUIAutomation> =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER);
            if let Some(el) = automation.ok().and_then(|a| a.GetFocusedElement().ok()) {
                let kind = el.CurrentControlType().map(|t| t.0).unwrap_or(0);
                let class: String = el.CurrentClassName().map(|c| c.to_string()).unwrap_or_default().chars().take(30).collect();
                log::info!("learning: watching a text box (type {kind}, class '{class}')");
                // Until the next dictation, a newer paste or the timeout,
                // keep the latest good reading: the field at an Enter, or
                // the field once it holds still between two checks.
                let deadline = Instant::now() + CHECK_AFTER;
                let mut good: Option<(String, &str)> = None;
                let mut prev: Option<String> = None;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(left.min(POLL)) {
                        Ok(Cmd::Snapshot(done)) => {
                            let text = read_field(&el);
                            let _ = done.send(());
                            if let Some(text) = text.filter(|t| has_words(t)) {
                                good = Some((text, "at Enter"));
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            let text = read_field(&el);
                            if let Some(t) = text.as_ref().filter(|t| has_words(t) && prev.as_ref() == Some(t)) {
                                good = Some((t.clone(), "while it held still"));
                            }
                            prev = text;
                        }
                        Ok(Cmd::Check) | Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                if let Some(on_learn) = ON_LEARN.get() {
                    check(&pasted, read_field(&el), good, on_learn.as_ref());
                }
            }
        }
        CoUninitialize();
    });
    if let Err(e) = spawned {
        log::warn!("correction learning: {e}");
    }
}

unsafe fn read_field(el: &IUIAutomationElement) -> Option<String> {
    if el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(true) {
        return None;
    }
    if let Ok(vp) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
        if let Ok(v) = vp.CurrentValue() {
            let s = v.to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    let tp = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId).ok()?;
    let range = tp.DocumentRange().ok()?;
    range.GetText(MAX_FIELD_CHARS).ok().map(|s| s.to_string())
}

/// At least half the pasted words are still in the field.
fn holds_paste(pasted: &str, field: &str) -> bool {
    let words: Vec<String> = field.split_whitespace().map(|w| core(w).to_lowercase()).collect();
    let p: Vec<String> = pasted.split_whitespace().map(|w| core(w).to_lowercase()).collect();
    let kept = p.iter().filter(|w| words.contains(w)).count();
    !p.is_empty() && kept * 2 >= p.len()
}

fn has_words(text: &str) -> bool {
    text.split_whitespace().next().is_some()
}

/// Uses the field as it is now, unless the paste has gone from it (the
/// message was sent, or the box was cleared) and there's an earlier good
/// reading. Logs why nothing was learned, without any of the text.
fn check(pasted: &str, now: Option<String>, good: Option<(String, &str)>, on_learn: &dyn Fn(String, String)) {
    let (field, when) = match (now, good) {
        (Some(now), _) if holds_paste(pasted, &now) => (now, "now"),
        (_, Some(good)) => good,
        (Some(now), None) => (now, "now"),
        (None, None) => return log::info!("learning: couldn't read the text box"),
    };
    let found = find_corrections(pasted, &field);
    if found.is_empty() {
        log::info!(
            "learning: no fix found ({} words pasted, {} in the box, read {when})",
            pasted.split_whitespace().count(),
            field.split_whitespace().count()
        );
    }
    for (heard, word) in found {
        log::info!("learned a spelling ({} chars, read {when})", word.chars().count());
        on_learn(heard, word);
    }
}

/// Common words never treated as a misheard spelling; changing one of
/// these is a grammar edit, and learning it would rewrite every future use.
const COMMON: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "so", "to", "of", "in", "on", "at", "for", "with", "is", "it",
    "be", "are", "was", "were", "i", "you", "he", "she", "we", "they", "this", "that", "there", "their",
    "then", "than", "your", "you're", "its", "it's", "not", "no", "yes", "do", "does", "did", "have",
    "has", "had", "can", "will", "would", "should", "could", "my", "me", "our", "us", "if", "as", "by",
    // One-word chat replies people restyle ("Thanks" -> "Thx", "Thank you").
    "thanks", "thank", "thx", "okay", "ok", "sure", "yeah", "yep", "nope", "cool", "great", "nice", "hi",
    "hey", "hello", "bye", "please", "sorry", "lol", "fine", "done", "alright",
];

fn core(w: &str) -> &str {
    w.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Looks like a name or term: starts with a capital (Niamh, Supabase) or
/// has one inside (GitHub, macOS).
fn looks_like_term(w: &str) -> bool {
    let mut chars = w.chars();
    let first_upper = chars.next().is_some_and(char::is_uppercase);
    first_upper || chars.any(char::is_uppercase)
}

/// Edit distance, for "does the fix sound like what was heard?".
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb { prev } else { 1 + prev.min(row[j]).min(row[j + 1]) };
            prev = cur;
        }
    }
    row[b.len()]
}

/// A mishearing keeps the first sound and most of the shape ("Neve" ->
/// "Niamh", "supa base" -> "Supabase"); swapping Friday for Monday is an
/// edit to the meaning, not a spelling fix.
fn sounds_alike(heard: &str, word: &str) -> bool {
    let (a, b) = (letters(heard), letters(word));
    if a.chars().next() != b.chars().next() {
        return false;
    }
    let longest = a.chars().count().max(b.chars().count()).max(1);
    levenshtein(&a, &b) * 10 <= longest * 8
}

/// Lowercase letters and digits only: "Supa base" -> "supabase".
fn letters(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// At either end of the paste the field may go on with other text, so the
/// typed words are the one to three that read most like what was heard.
fn closest_span(heard: &str, f: &[&str], spans: impl Iterator<Item = Range<usize>>) -> Option<Range<usize>> {
    let h = letters(heard);
    spans.min_by_key(|r| levenshtein(&h, &letters(&f[r.clone()].concat())))
}

const SMALL_NUMBERS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve",
    "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS: &[&str] = &["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

/// "fifty" -> 50, "twenty-one" -> 21, "five hundred" -> 500.
fn words_to_number(words: &[&str]) -> Option<u64> {
    let (mut total, mut current, mut any) = (0u64, 0u64, false);
    for w in words.iter().flat_map(|w| w.split('-')) {
        let w = w.to_lowercase();
        if let Some(n) = SMALL_NUMBERS.iter().position(|s| *s == w) {
            current += n as u64;
        } else if let Some(n) = TENS.iter().position(|s| *s == w) {
            current += (n as u64 + 2) * 10;
        } else {
            match w.as_str() {
                "hundred" => current = current.max(1) * 100,
                "thousand" | "million" => {
                    let scale = if w == "thousand" { 1_000 } else { 1_000_000 };
                    total += current.max(1) * scale;
                    current = 0;
                }
                "and" if any => continue,
                _ => return None,
            }
        }
        any = true;
    }
    any.then_some(total + current)
}

/// "50" or "2,000" as a number.
fn digits_value(w: &str) -> Option<u64> {
    let d: String = w.chars().filter(|c| *c != ',').collect();
    if d.is_empty() || !d.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    d.parse().ok()
}

/// A run of pasted words and what the user typed in its place. `left` and
/// `right` index the matched pairs on either side, when there is one.
struct Gap {
    heard: Range<usize>,
    fixed: Range<usize>,
    left: Option<usize>,
    right: Option<usize>,
}

/// A spoken number the user rewrote as digits inside a name ("Nifty fifty
/// Solutions" -> "Nifty 50 Solutions", "Windows eleven" -> "Windows 11").
/// The whole name is learned, so "fifty" elsewhere is left alone. A number
/// with no name around it is just style, and Smart handles that.
fn number_fix(p: &[&str], f: &[&str], pairs: &[(usize, usize)], gap: &Gap) -> Option<(String, String)> {
    if gap.heard.len() > 3 || gap.fixed.len() != 1 {
        return None;
    }
    let value = words_to_number(&p[gap.heard.clone()])?;
    if digits_value(f[gap.fixed.start])? != value {
        return None;
    }
    // Take in up to three name-like words on each side.
    let (mut ps, mut fs) = (gap.heard.start, gap.fixed.start);
    if let Some(mut k) = gap.left {
        for _ in 0..3 {
            let (pk, fk) = pairs[k];
            if pk + 1 != ps || fk + 1 != fs || !looks_like_term(f[fk]) {
                break;
            }
            (ps, fs) = (pk, fk);
            if k == 0 {
                break;
            }
            k -= 1;
        }
    }
    let (mut pe, mut fe) = (gap.heard.end, gap.fixed.end);
    if let Some(mut k) = gap.right {
        for _ in 0..3 {
            if k >= pairs.len() || pairs[k] != (pe, fe) || !looks_like_term(f[fe]) {
                break;
            }
            (pe, fe) = (pe + 1, fe + 1);
            k += 1;
        }
    }
    if ps == gap.heard.start && pe == gap.heard.end {
        return None;
    }
    Some((p[ps..pe].join(" "), f[fs..fe].join(" ")))
}

/// A misheard name or term swapped for the right one, one to three words
/// on either side: "Neve" -> "Niamh", "supa base" -> "Supabase",
/// "Superbase" -> "Supa base". The fix must start with a capital, keep the
/// first sound, and not replace a common word: retyping a one-word "Thanks"
/// as "Thx" is a style change, not a mishearing.
fn term_fix(p: &[&str], f: &[&str], gap: &Gap) -> Option<(String, String)> {
    let heard = &p[gap.heard.clone()];
    let fixed = &f[gap.fixed.clone()];
    if heard.len() > 3 || fixed.len() > 3 {
        return None;
    }
    let heard_text = heard.join(" ");
    let word = fixed.join(" ");
    let ok = letters(&word).chars().count() >= 3
        && fixed.iter().any(|w| looks_like_term(w))
        && fixed.iter().all(|w| w.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '\''))
        && !heard_text.eq_ignore_ascii_case(&word)
        && sounds_alike(&heard_text, &word)
        && !(heard.len() == 1 && COMMON.contains(&heard[0].to_lowercase().as_str()));
    ok.then_some((heard_text, word))
}

/// The same word with its capitals fixed into a mixed-case name ("github"
/// -> "GitHub", "macos" -> "macOS"). All-caps edits are skipped: "VERY" is
/// usually emphasis, not a spelling.
fn case_fix(heard: &str, word: &str) -> Option<(String, String)> {
    let inner_cap = word.chars().skip(1).any(char::is_uppercase) && word.chars().any(char::is_lowercase);
    let ok = heard != word && inner_cap && word.chars().count() >= 3 && !COMMON.contains(&heard.to_lowercase().as_str());
    ok.then(|| (heard.to_string(), word.to_string()))
}

/// What the user fixed in place, inside a sentence that otherwise survived
/// intact: a misheard name, a spoken number inside a name, or a name's
/// capitals.
pub fn find_corrections(pasted: &str, field: &str) -> Vec<(String, String)> {
    let p: Vec<&str> = pasted.split_whitespace().map(core).filter(|w| !w.is_empty()).collect();
    let f_all: Vec<&str> = field.split_whitespace().map(core).filter(|w| !w.is_empty()).collect();
    // A paste of one or two words has no words around a fix to line up
    // with, so it's only checked when the box holds little else ("Supa
    // base" sent on its own and fixed to "Supabase").
    let short = p.len() < 3;
    if p.is_empty() || p.len() > 300 || f_all.is_empty() || (short && f_all.len() > p.len() + 1) {
        return vec![];
    }
    let f = &f_all[f_all.len().saturating_sub(4000)..];
    let (n, m) = (p.len(), f.len());
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    // Longest common subsequence, then walk it back into matched pairs.
    let mut dp = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if eq(p[i], f[j]) { dp[at(i + 1, j + 1)] + 1 } else { dp[at(i + 1, j)].max(dp[at(i, j + 1)]) };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if eq(p[i], f[j]) {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // The pasted sentence must have survived mostly intact, or it was a rewrite.
    if !short && pairs.len() * 10 < n * 7 {
        return vec![];
    }

    // The runs between matched words. At either end of the paste the field
    // may go on with other text, so only one typed word is considered there.
    let mut gaps = Vec::new();
    if pairs.is_empty() {
        gaps.push(Gap { heard: 0..n, fixed: 0..m, left: None, right: None });
    } else {
        let (first, last) = (pairs[0], pairs[pairs.len() - 1]);
        if first.0 > 0 && first.1 > 0 {
            let heard = p[..first.0].join(" ");
            if let Some(fixed) = closest_span(&heard, f, (1..=first.1.min(3)).map(|k| first.1 - k..first.1)) {
                gaps.push(Gap { heard: 0..first.0, fixed, left: None, right: Some(0) });
            }
        }
        for (k, w) in pairs.windows(2).enumerate() {
            let ((pi, fj), (pi2, fj2)) = (w[0], w[1]);
            gaps.push(Gap { heard: pi + 1..pi2, fixed: fj + 1..fj2, left: Some(k), right: Some(k + 1) });
        }
        if last.0 + 1 < n && last.1 + 1 < m {
            let heard = p[last.0 + 1..].join(" ");
            let spans = (1..=(m - last.1 - 1).min(3)).map(|k| last.1 + 1..last.1 + 1 + k);
            if let Some(fixed) = closest_span(&heard, f, spans) {
                gaps.push(Gap { heard: last.0 + 1..n, fixed, left: Some(pairs.len() - 1), right: None });
            }
        }
    }

    let mut found: Vec<(String, String)> = gaps
        .iter()
        .filter(|g| !g.heard.is_empty() && !g.fixed.is_empty())
        .filter_map(|g| number_fix(&p, f, &pairs, g).or_else(|| term_fix(&p, f, g)))
        .collect();
    found.extend(pairs.iter().filter_map(|&(pi, fj)| case_fix(p[pi], f[fj])));
    found.dedup();
    found.truncate(MAX_LEARN_PER_PASTE);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_a_name_fixed_in_place() {
        let got = find_corrections("Thanks Neve, talk soon about the launch.", "Thanks Niamh, talk soon about the launch.");
        assert_eq!(got, vec![("Neve".to_string(), "Niamh".to_string())]);
    }

    #[test]
    fn learns_a_two_word_mishearing() {
        let got = find_corrections("I pushed the fix to supa base this morning.", "I pushed the fix to Supabase this morning.");
        assert_eq!(got, vec![("supa base".to_string(), "Supabase".to_string())]);
    }

    #[test]
    fn finds_the_paste_inside_a_longer_field() {
        let field = "Earlier notes here. Then I deployed it to Tauri builds today. More text after.";
        let got = find_corrections("Then I deployed it to Tory builds today.", field);
        assert_eq!(got, vec![("Tory".to_string(), "Tauri".to_string())]);
    }

    #[test]
    fn learns_a_number_inside_a_name() {
        let got = find_corrections("I work at Nifty fifty Solutions now.", "I work at Nifty 50 Solutions now.");
        assert_eq!(got, vec![("Nifty fifty Solutions".to_string(), "Nifty 50 Solutions".to_string())]);
        let got = find_corrections("We finally moved to Windows eleven.", "We finally moved to Windows 11. Next line.");
        assert_eq!(got, vec![("Windows eleven".to_string(), "Windows 11".to_string())]);
    }

    #[test]
    fn learns_a_short_message_fixed_on_its_own() {
        let got = find_corrections("Supa base", "Supabase");
        assert_eq!(got, vec![("Supa base".to_string(), "Supabase".to_string())]);
        let got = find_corrections("Nifty fifty.", "Nifty 50");
        assert_eq!(got, vec![("Nifty fifty".to_string(), "Nifty 50".to_string())]);
        // A one-word restyle, or a short paste inside other typed text, isn't learned.
        assert!(find_corrections("Thanks.", "Thx").is_empty());
        assert!(find_corrections("Thanks.", "Thank you").is_empty());
        // Speech recognition can be far off on one word; the fix still counts.
        let got = find_corrections("Superbus.", "Supabase");
        assert_eq!(got, vec![("Superbus".to_string(), "Supabase".to_string())]);
        assert!(find_corrections("Supa base", "Earlier I wrote about Supabase").is_empty());
    }

    #[test]
    fn learns_one_word_split_into_two() {
        let got = find_corrections("Superbase.", "Supa base");
        assert_eq!(got, vec![("Superbase".to_string(), "Supa base".to_string())]);
        let got = find_corrections("We moved the auth to superbase.", "We moved the auth to Supa base. More text");
        assert_eq!(got, vec![("superbase".to_string(), "Supa base".to_string())]);
        let got = find_corrections("Superbase", "Supabase");
        assert_eq!(got, vec![("Superbase".to_string(), "Supabase".to_string())]);
        // Split into ordinary lowercase words: a grammar edit, not a name.
        assert!(find_corrections("We should meet anyway tomorrow.", "We should meet any way tomorrow.").is_empty());
    }

    #[test]
    fn a_sent_message_is_judged_by_the_reading_at_enter() {
        assert!(holds_paste("I pushed it to supa base.", "I pushed it to Supabase."));
        assert!(!holds_paste("I pushed it to supa base.", ""));
        assert!(!holds_paste("Supa base", "Supabase"));
    }

    #[test]
    fn a_plain_number_edit_is_not_learned() {
        assert!(find_corrections("We have fifty signups today.", "We have 50 signups today.").is_empty());
    }

    #[test]
    fn learns_a_fix_at_either_end() {
        let got = find_corrections("Neve, can you call me back?", "Niamh, can you call me back?");
        assert_eq!(got, vec![("Neve".to_string(), "Niamh".to_string())]);
    }

    #[test]
    fn learns_mixed_case_names_but_not_emphasis() {
        let got = find_corrections("I pushed it to github this morning.", "I pushed it to GitHub this morning.");
        assert_eq!(got, vec![("github".to_string(), "GitHub".to_string())]);
        assert!(find_corrections("This is very important for us.", "This is VERY important for US.").is_empty());
    }

    #[test]
    fn number_words() {
        assert_eq!(words_to_number(&["fifty"]), Some(50));
        assert_eq!(words_to_number(&["twenty-one"]), Some(21));
        assert_eq!(words_to_number(&["five", "hundred"]), Some(500));
        assert_eq!(words_to_number(&["two", "thousand", "and", "five"]), Some(2005));
        assert_eq!(words_to_number(&["lots"]), None);
        assert_eq!(digits_value("2,000"), Some(2000));
    }

    #[test]
    fn ignores_grammar_edits_and_rewrites() {
        assert!(find_corrections("I think there going to ship it.", "I think they're going to ship it.").is_empty());
        assert!(find_corrections("Send it over by Friday please.", "Send it over by Monday please.").is_empty());
        assert!(find_corrections("We should ship it on Friday.", "Totally different message now.").is_empty());
        assert!(find_corrections("We should ship it on Friday.", "We should ship it on Friday.").is_empty());
    }
}
