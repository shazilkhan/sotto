//! Learns spellings from the user's own fixes. Right after a paste, the
//! focused text field is remembered. At the next dictation (or after 25 s)
//! it's read once more, and a name the user retyped in place ("Neve" ->
//! "Niamh") becomes a Dictionary entry.
//!
//! Privacy: only the field that was pasted into is read, only once, never
//! a password field, and nothing is kept except the learned word pair. UI
//! Automation calls run on their own thread, so a slow or hung app can't
//! stall dictation.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationValuePattern,
    UIA_TextPatternId, UIA_ValuePatternId,
};

/// How long after a paste to look for a fix, if no new dictation comes first.
const CHECK_AFTER: Duration = Duration::from_secs(25);
const MAX_FIELD_CHARS: i32 = 20_000;
const MAX_LEARN_PER_PASTE: usize = 2;

type OnLearn = Box<dyn Fn(String, String) + Send + Sync>;
static ON_LEARN: OnceLock<OnLearn> = OnceLock::new();
/// Wakes the watcher for the latest paste so it checks right away.
static ACTIVE: Mutex<Option<Sender<()>>> = Mutex::new(None);

/// `on_learn(heard, word)` is called for each spelling the user fixed.
pub fn start(on_learn: OnLearn) {
    let _ = ON_LEARN.set(on_learn);
}

/// Checks the last paste now (called when a new dictation starts).
pub fn check_now() {
    if let Some(tx) = ACTIVE.lock().unwrap().take() {
        let _ = tx.send(());
    }
}

/// Remembers the focused field for this paste. The accessibility client
/// lives only on this short-lived thread: creating it starts a pool of
/// Windows worker threads, so it's never kept around while idle.
pub fn watch(pasted: &str) {
    check_now();
    let (tx, rx) = channel::<()>();
    *ACTIVE.lock().unwrap() = Some(tx);
    let pasted = pasted.trim().to_string();
    let spawned = std::thread::Builder::new().name("learn".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        {
            let automation: windows::core::Result<IUIAutomation> =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER);
            if let Some(el) = automation.ok().and_then(|a| a.GetFocusedElement().ok()) {
                // Woken by the next dictation, the timeout, or a newer paste.
                let _ = rx.recv_timeout(CHECK_AFTER);
                if let Some(on_learn) = ON_LEARN.get() {
                    check(&(el, pasted), on_learn.as_ref());
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

fn check((el, pasted): &(IUIAutomationElement, String), on_learn: &dyn Fn(String, String)) {
    let Some(field) = (unsafe { read_field(el) }) else { return };
    for (heard, word) in find_corrections(pasted, &field) {
        log::info!("learned a spelling ({} chars)", word.chars().count());
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
    let a: String = heard.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    let b = word.to_lowercase();
    if a.chars().next() != b.chars().next() {
        return false;
    }
    let longest = a.chars().count().max(b.chars().count()).max(1);
    levenshtein(&a, &b) * 10 <= longest * 8
}

/// Words the user replaced in place: one or two pasted words swapped for a
/// single term, inside a sentence that otherwise survived intact.
pub fn find_corrections(pasted: &str, field: &str) -> Vec<(String, String)> {
    let p: Vec<&str> = pasted.split_whitespace().map(core).filter(|w| !w.is_empty()).collect();
    let f_all: Vec<&str> = field.split_whitespace().map(core).filter(|w| !w.is_empty()).collect();
    if p.len() < 3 || p.len() > 300 || f_all.is_empty() {
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
    if pairs.len() * 10 < n * 7 {
        return vec![];
    }

    let mut found = Vec::new();
    for w in pairs.windows(2) {
        let ((pi, fj), (pi2, fj2)) = (w[0], w[1]);
        let heard = &p[pi + 1..pi2];
        let fixed = &f[fj + 1..fj2];
        if heard.is_empty() || heard.len() > 2 || fixed.len() != 1 {
            continue;
        }
        let word = fixed[0];
        let heard_text = heard.join(" ");
        let ok = word.chars().count() >= 3
            && looks_like_term(word)
            && word.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '\'')
            && !heard_text.eq_ignore_ascii_case(word)
            && sounds_alike(&heard_text, word)
            && !(heard.len() == 1 && COMMON.contains(&heard[0].to_lowercase().as_str()));
        if ok {
            found.push((heard_text, word.to_string()));
        }
        if found.len() == MAX_LEARN_PER_PASTE {
            break;
        }
    }
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
    fn ignores_grammar_edits_and_rewrites() {
        assert!(find_corrections("I think there going to ship it.", "I think they're going to ship it.").is_empty());
        assert!(find_corrections("Send it over by Friday please.", "Send it over by Monday please.").is_empty());
        assert!(find_corrections("We should ship it on Friday.", "Totally different message now.").is_empty());
        assert!(find_corrections("We should ship it on Friday.", "We should ship it on Friday.").is_empty());
    }
}
