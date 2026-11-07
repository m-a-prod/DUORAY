//! The core's interface strings (errors and names the GUI shows) are written
//! in Russian and go through the translator the GUI sets, which holds the
//! translations; Russian until it does. Country names have their own English
//! table (see `balancer::country_name`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static TRANSLATE: OnceLock<fn(&'static str) -> &'static str> = OnceLock::new();
static ENGLISH: AtomicBool = AtomicBool::new(false);

/// The GUI's translator, set once at start.
pub fn set_translator(translate: fn(&'static str) -> &'static str) {
    let _ = TRANSLATE.set(translate);
}

/// The interface language changed.
pub fn set_english(english: bool) {
    ENGLISH.store(english, Ordering::Relaxed);
}

pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// `ru` in the interface language.
pub fn tr(ru: &'static str) -> &'static str {
    TRANSLATE.get().map_or(ru, |translate| translate(ru))
}

/// `tr` for a template with named `{placeholders}`, filled after
/// translating.
#[macro_export]
macro_rules! core_trf {
    ($t:literal $(, $k:ident = $v:expr)* $(,)?) => {{
        #[allow(unused_mut)]
        let mut s = $crate::i18n::tr($t).to_string();
        $( s = s.replace(concat!("{", stringify!($k), "}"), &($v).to_string()); )*
        s
    }};
}

/// Marks a string for translation where it cannot be translated yet (a
/// const): it is `tr`-ed where shown.
pub const fn n(ru: &'static str) -> &'static str {
    ru
}
