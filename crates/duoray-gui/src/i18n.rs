//! The interface language. Strings are written in Russian; English comes
//! from translations/en/LC_MESSAGES/duoray-gui.po, which slint-build bundles
//! for the .slint UI (`@tr`) and this module reads for strings made in Rust.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// Russian → English, read once from the bundled .po.
static EN: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| parse_po(include_str!("../translations/en/LC_MESSAGES/duoray-gui.po")));

/// The setting ("" as the system, "ru", "en") to the language shown.
pub fn english_for(setting: &str) -> bool {
    match setting {
        "en" => true,
        "ru" => false,
        _ => !sys_locale::get_locale().is_some_and(|l| speaks_russian(&l)),
    }
}

/// Locales whose people mostly read Russian more easily than English.
fn speaks_russian(locale: &str) -> bool {
    let base = locale.split(['-', '_', '.', '@']).next().unwrap_or_default().to_ascii_lowercase();
    ["ru", "be", "kk", "ky", "tg", "uz"].contains(&base.as_str())
}

/// Switches the interface: the bundled .slint translation and the strings
/// made here from now on.
pub fn set(english: bool) {
    ENGLISH.store(english, Ordering::Relaxed);
    // The core's strings and country names follow.
    duoray_core::i18n::set_translator(tr);
    duoray_core::i18n::set_english(english);
    let _ = slint::select_bundled_translation(if english { "en" } else { "" });
}

pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Marks a string for translation where it cannot be translated yet (a
/// const, read once): it is `tr`-ed where shown. update.py collects it.
pub const fn n(ru: &'static str) -> &'static str {
    ru
}

/// `ru` in the interface language (itself when there is no translation).
pub fn tr(ru: &'static str) -> &'static str {
    if !english() {
        return ru;
    }
    EN.get(ru).copied().unwrap_or(ru)
}

/// `tr` for a template with named `{placeholders}`, filled after
/// translating: `trf!("Добавлено: {n}", n = count)`.
#[macro_export]
macro_rules! trf {
    ($t:literal $(, $k:ident = $v:expr)* $(,)?) => {{
        #[allow(unused_mut)]
        let mut s = $crate::i18n::tr($t).to_string();
        $( s = s.replace(concat!("{", stringify!($k), "}"), &($v).to_string()); )*
        s
    }};
}

/// "5 серверов" / "5 servers": Russian picks among three forms, English
/// between the translations of the first and the last.
pub fn plural(n: usize, one: &'static str, few: &'static str, many: &'static str) -> String {
    let word = if english() {
        if n == 1 { tr(one) } else { tr(many) }
    } else {
        match (n % 10, n % 100) {
            (1, r) if r != 11 => one,
            (2..=4, r) if !(12..=14).contains(&r) => few,
            _ => many,
        }
    };
    format!("{n} {word}")
}

/// msgid → msgstr of a gettext file (single or continued lines; untranslated
/// entries left out). Leaked: read once for the life of the program.
fn parse_po(text: &str) -> HashMap<&'static str, &'static str> {
    fn unquote(line: &str) -> Option<String> {
        let s = line.trim().strip_prefix('"')?.strip_suffix('"')?;
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        Some(out)
    }
    let mut map = HashMap::new();
    let (mut id, mut text_out, mut field) = (String::new(), String::new(), 0);
    let mut flush = |id: &mut String, text_out: &mut String| {
        if !id.is_empty() && !text_out.is_empty() {
            map.insert(&*String::leak(std::mem::take(id)), &*String::leak(std::mem::take(text_out)));
        }
        id.clear();
        text_out.clear();
    };
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("msgid ") {
            flush(&mut id, &mut text_out);
            id = unquote(rest).unwrap_or_default();
            field = 1;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            text_out = unquote(rest).unwrap_or_default();
            field = 2;
        } else if line.starts_with('"') {
            let part = unquote(line).unwrap_or_default();
            match field {
                1 => id.push_str(&part),
                2 => text_out.push_str(&part),
                _ => {}
            }
        } else if line.is_empty() || line.starts_with('#') {
            field = 0;
        }
    }
    flush(&mut id, &mut text_out);
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_po_entries() {
        let po = "msgid \"\"\nmsgstr \"Content-Type: text/plain\"\n\n#: x\nmsgid \"Подключено\"\nmsgstr \"Connected\"\n\nmsgid \"Две\"\n\"строки\"\nmsgstr \"Two\"\n\" lines\"\n\nmsgid \"Нет перевода\"\nmsgstr \"\"\n";
        let map = parse_po(po);
        assert_eq!(map.get("Подключено"), Some(&"Connected"));
        assert_eq!(map.get("Двестроки"), Some(&"Two lines"));
        assert!(!map.contains_key("Нет перевода") && !map.contains_key(""));
    }

    #[test]
    fn russian_speaking_locales() {
        assert!(speaks_russian("ru_RU.UTF-8") && speaks_russian("kk-KZ") && speaks_russian("ru"));
        assert!(!speaks_russian("en-US") && !speaks_russian("de_DE"));
    }
}
