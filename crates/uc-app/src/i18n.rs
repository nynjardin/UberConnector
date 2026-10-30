//! Interface language: French, or English for everyone else. Follows the
//! Windows display language unless the user picked one in the settings.
//! Texts are written in both languages where they are used, see `tr!`.

use std::sync::atomic::{AtomicBool, Ordering};

static FRENCH: AtomicBool = AtomicBool::new(true);

/// Applies the `lang` setting: "fr", "en", or anything else for automatic.
pub fn set(lang: &str) {
    let french = match lang {
        "fr" => true,
        "en" => false,
        _ => windows_is_french(),
    };
    FRENCH.store(french, Ordering::Relaxed);
}

pub fn french() -> bool {
    FRENCH.load(Ordering::Relaxed)
}

/// "fr" or "en", for the interface.
pub fn code() -> &'static str {
    if french() { "fr" } else { "en" }
}

fn windows_is_french() -> bool {
    const LANG_FRENCH: u16 = 0x0c;
    // Primary language in the low 10 bits of the LANGID (fr-FR, fr-CA, fr-BE...).
    unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() & 0x3ff == LANG_FRENCH }
}

/// `tr!("français", "English")`: the text in the current language, as a
/// `&'static str`; with `{name}` captures, as a `String` (via `format!`).
macro_rules! tr {
    ($fr:literal, $en:literal) => {
        if $crate::i18n::french() { $fr } else { $en }
    };
}

macro_rules! trf {
    ($fr:literal, $en:literal) => {
        if $crate::i18n::french() { format!($fr) } else { format!($en) }
    };
    ($fr:literal, $en:literal, $($arg:expr),+) => {
        if $crate::i18n::french() { format!($fr, $($arg),+) } else { format!($en, $($arg),+) }
    };
}

pub(crate) use {tr, trf};
