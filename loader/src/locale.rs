//! Display language for mods and overlay chrome.
//!
//! Detection is shared with the Weblate tables: the game's own
//! language choice (`Binaries\SETTINGS\GCUSERSETTINGSDATA.MXML`,
//! which the game rewrites itself), else the Steam client language
//! (`steamapps\appmanifest_275850.acf`), else English
//! (`nozavolochen_strings::detect_in`).
//!
//! Two layers share that language:
//! - overlay chrome (`loader/strings/<code>.json` via the
//!   `nozavolochen-strings` crate, English fallback built in);
//! - mod-provided strings: `apply_locale` swaps a spec's
//!   labels/descriptions for matching `[[mod.text]]` /
//!   `[[setting.text]]` entries; anything unmatched stays in source
//!   English, so partial translations degrade cleanly.

use std::sync::Mutex;

use crate::spec::Mod;

/// Cached display language, refreshed on every overlay open: NMS
/// hot-applies language changes without a restart, so boot-time
/// detection goes stale.
static LOCALE: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

/// Re-resolve the display language and cache it. Returns `(code, source)`.
pub fn refresh() -> (String, String) {
    let (code, source) = match crate::host::game_root() {
        Some(root) => {
            let (c, s) = nozavolochen_strings::detect_in(&root);
            (c.to_string(), s.to_string())
        }
        None => ("en".to_string(), "fallback".to_string()),
    };
    if let Ok(mut guard) = LOCALE.lock() {
        *guard = (code.clone(), source.clone());
    }
    (code, source)
}

/// The cached display language, detecting once when empty.
pub fn current() -> (String, String) {
    let cached = LOCALE.lock().map(|g| g.clone()).unwrap_or_default();
    if cached.0.is_empty() {
        return refresh();
    }
    cached
}

/// Snapshot specs with this process's display language applied.
/// Snapshots stay pristine English; only these clones are swapped.
pub fn localized_mods() -> Vec<Mod> {
    let mut mods = crate::host::current_mods();
    for m in &mut mods {
        apply_locale(m);
    }
    mods
}

/// Swap matching translated labels/descriptions into the spec.
/// Last matching entry per language wins.
pub fn apply_locale(spec: &mut Mod) {
    let (code, _) = current();
    apply_locale_for(spec, &code);
}

/// Same as [`apply_locale`] with an explicit locale code (for tests).
pub fn apply_locale_for(spec: &mut Mod, code: &str) {
    for t in &spec.texts {
        if t.lang.eq_ignore_ascii_case(code) {
            if !t.label.is_empty() {
                spec.label.clone_from(&t.label);
            }
            if !t.description.is_empty() {
                spec.description.clone_from(&t.description);
            }
        }
    }
    for s in &mut spec.settings {
        for t in &s.texts {
            if t.lang.eq_ignore_ascii_case(code) {
                if !t.label.is_empty() {
                    s.label.clone_from(&t.label);
                }
                if !t.description.is_empty() {
                    s.description.clone_from(&t.description);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_falls_back_without_game_files() {
        // Test env has no game dir: English fallback, and apply keeps
        // source strings through the public entry point.
        assert_eq!(refresh(), ("en".to_string(), "fallback".to_string()));
        assert_eq!(current(), ("en".to_string(), "fallback".to_string()));
    }

    const APPLY_TOML: &str = r#"
[mod]
id = "x"
label = "Source Label"
description = "Source byline."

[[mod.text]]
lang = "nl"
description = "Bronregel."

[[setting]]
id = "feat"
kind = "bool"
label = "Source row"
description = "Source explain."
default = true

[[setting.text]]
lang = "nl"
label = "Bronrij"
description = "Bronuitleg."

[[setting.text]]
lang = "PT-br"
label = "BRrij"
"#;

    #[test]
    fn apply_swaps_matching_lang_only() {
        let mut m = crate::spec::parse_mod(APPLY_TOML).unwrap();
        // Unknown locale: everything stays source English.
        apply_locale_for(&mut m, "xx");
        assert_eq!(m.label, "Source Label");
        assert_eq!(m.settings[0].label, "Source row");
        // Dutch: empty mod label keeps the source; fields fill in.
        apply_locale_for(&mut m, "nl");
        assert_eq!(m.label, "Source Label");
        assert_eq!(m.description, "Bronregel.");
        assert_eq!(m.settings[0].label, "Bronrij");
        assert_eq!(m.settings[0].description, "Bronuitleg.");
        // Codes compare case-insensitively (parsed langs are lowercase).
        apply_locale_for(&mut m, "pt-BR");
        assert_eq!(m.settings[0].label, "BRrij");
    }

    #[test]
    fn chrome_comes_from_string_tables() {
        // `loader/strings/en.json` is the key set; every supported
        // language resolves every key (English fallback underneath).
        let en = nozavolochen_strings::Strings::load("en", None);
        assert_eq!(en.get("chrome.switch"), "switch");
        for lang in nozavolochen_strings::SUPPORTED {
            let t = nozavolochen_strings::Strings::load(lang, None);
            for key in [
                "chrome.on",
                "chrome.off",
                "chrome.select",
                "chrome.adjust",
                "chrome.ok",
                "chrome.reset",
                "chrome.switch",
                "chrome.close",
            ] {
                assert!(!t.get(key).is_empty(), "{lang}/{key}");
            }
        }
        // Unknown languages fall back to English.
        let t = nozavolochen_strings::Strings::load("xx", None);
        assert_eq!(t.get("chrome.on"), "on");
    }
}
