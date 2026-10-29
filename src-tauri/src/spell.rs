//! Windows' built-in spell checker, used only to tell a real word ("base")
//! from a misheard one ("Biss") before Light swaps in a Dictionary
//! spelling. It's created when a near-miss turns up and dropped right
//! after, so nothing stays loaded while idle.

use windows::core::HSTRING;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Globalization::{ISpellChecker, ISpellCheckerFactory, SpellCheckerFactory};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

/// Whether each word is in the English dictionary. `None` when no English
/// spell checker is installed, so callers treat every word as real.
pub fn known(words: &[&str]) -> Option<Vec<bool>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: ISpellCheckerFactory = CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let lang = ["en-US", "en-GB"]
            .into_iter()
            .find(|l| factory.IsSupported(&HSTRING::from(*l)).map(|b| b.as_bool()).unwrap_or(false))?;
        let checker: ISpellChecker = factory.CreateSpellChecker(&HSTRING::from(lang)).ok()?;
        Some(
            words
                .iter()
                .map(|w| {
                    let Ok(errors) = checker.Check(&HSTRING::from(*w)) else { return true };
                    let mut first = None;
                    !(errors.Next(&mut first) == S_OK && first.is_some())
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tells_real_words_from_mishearings() {
        // Compounds like "Superbase" pass as words, so those go to Smart.
        let Some(got) = super::known(&["base", "super", "Biss"]) else { return };
        assert_eq!(got, vec![true, true, false]);
    }
}
