//! Overlay display language for mod-provided strings.
//!
//! Resolved once per process: the game's own language choice
//! (`Binaries\SETTINGS\GCUSERSETTINGSDATA.MXML`, which the game
//! rewrites itself), else the Steam client's language
//! (`steamapps\appmanifest_275850.acf`), else English.
//! `apply_locale` swaps a spec's labels/descriptions for matching
//! `[[mod.text]]` / `[[setting.text]]` entries; anything unmatched
//! stays in source English, so partial translations degrade cleanly.

use std::sync::Mutex;

use crate::spec::Mod;

/// Cached display language, refreshed on every overlay open: NMS
/// hot-applies language changes without a restart, so boot-time
/// detection goes stale.
static LOCALE: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

/// Re-resolve the display language and cache it. Returns `(code, source)`.
pub fn refresh() -> (String, String) {
    let detected = detect();
    if let Ok(mut guard) = LOCALE.lock() {
        *guard = detected.clone();
    }
    detected
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

/// Framework chrome words (`on`/`off` markers, footer actions).
/// Only languages with a verified table translate; everything else
/// stays English. (The strings-table branch remains the long-term
/// Weblate home for these; this inline table covers what mods need
/// today without new dependencies.)
pub fn chrome(lang: &str, key: &str) -> &'static str {
    match (lang, key) {
        ("nl", "on") => "aan",
        ("nl", "off") => "uit",
        ("nl", "select") => "selecteren",
        ("nl", "adjust") => "aanpassen",
        ("nl", "ok") => "ok",
        ("nl", "reset") => "resetten",
        ("nl", "close") => "sluiten",
        ("de", "on") => "an",
        ("de", "off") => "aus",
        ("de", "select") => "auswählen",
        ("de", "adjust") => "anpassen",
        ("de", "ok") => "ok",
        ("de", "reset") => "zurücksetzen",
        ("de", "close") => "schließen",
        ("fr", "on") => "oui",
        ("fr", "off") => "non",
        ("fr", "select") => "sélectionner",
        ("fr", "adjust") => "ajuster",
        ("fr", "ok") => "ok",
        ("fr", "reset") => "réinitialiser",
        ("fr", "close") => "fermer",
        ("it", "on") => "sì",
        ("it", "off") => "no",
        ("it", "select") => "selezionare",
        ("it", "adjust") => "regolare",
        ("it", "ok") => "ok",
        ("it", "reset") => "reimpostare",
        ("it", "close") => "chiudere",
        ("es", "on") => "sí",
        ("es", "off") => "no",
        ("es", "select") => "seleccionar",
        ("es", "adjust") => "ajustar",
        ("es", "ok") => "ok",
        ("es", "reset") => "restablecer",
        ("es", "close") => "cerrar",
        ("es-419", "on") => "sí",
        ("es-419", "off") => "no",
        ("es-419", "select") => "seleccionar",
        ("es-419", "adjust") => "ajustar",
        ("es-419", "ok") => "ok",
        ("es-419", "reset") => "restablecer",
        ("es-419", "close") => "cerrar",
        ("ru", "on") => "да",
        ("ru", "off") => "нет",
        ("ru", "select") => "выбрать",
        ("ru", "adjust") => "настроить",
        ("ru", "ok") => "OK",
        ("ru", "reset") => "сбросить",
        ("ru", "close") => "закрыть",
        ("pl", "on") => "tak",
        ("pl", "off") => "nie",
        ("pl", "select") => "wybrać",
        ("pl", "adjust") => "dostosować",
        ("pl", "ok") => "OK",
        ("pl", "reset") => "zresetować",
        ("pl", "close") => "zamknąć",
        ("pt", "on") => "sim",
        ("pt", "off") => "não",
        ("pt", "select") => "selecionar",
        ("pt", "adjust") => "ajustar",
        ("pt", "ok") => "ok",
        ("pt", "reset") => "repor",
        ("pt", "close") => "fechar",
        ("pt-BR", "on") => "sim",
        ("pt-BR", "off") => "não",
        ("pt-BR", "select") => "selecionar",
        ("pt-BR", "adjust") => "ajustar",
        ("pt-BR", "ok") => "ok",
        ("pt-BR", "reset") => "redefinir",
        ("pt-BR", "close") => "fechar",
        ("ja", "on") => "オン",
        ("ja", "off") => "オフ",
        ("ja", "select") => "選択する",
        ("ja", "adjust") => "調整する",
        ("ja", "ok") => "OK",
        ("ja", "reset") => "リセット",
        ("ja", "close") => "閉じる",
        ("zh", "on") => "开",
        ("zh", "off") => "关",
        ("zh", "select") => "选择",
        ("zh", "adjust") => "调整",
        ("zh", "ok") => "确定",
        ("zh", "reset") => "重置",
        ("zh", "close") => "关闭",
        ("zh-Hant", "on") => "開",
        ("zh-Hant", "off") => "關",
        ("zh-Hant", "select") => "選擇",
        ("zh-Hant", "adjust") => "調整",
        ("zh-Hant", "ok") => "確定",
        ("zh-Hant", "reset") => "重設",
        ("zh-Hant", "close") => "關閉",
        ("ko", "on") => "켜짐",
        ("ko", "off") => "꺼짐",
        ("ko", "select") => "선택",
        ("ko", "adjust") => "조정",
        ("ko", "ok") => "확인",
        ("ko", "reset") => "재설정",
        ("ko", "close") => "닫기",
        (_, "on") => "on",
        (_, "off") => "off",
        (_, "select") => "select",
        (_, "adjust") => "adjust",
        (_, "ok") => "ok",
        (_, "reset") => "reset",
        (_, "close") => "close",
        _ => "",
    }
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

fn detect() -> (String, String) {
    if let Some(root) = crate::host::game_root() {
        let settings = root
            .join("Binaries")
            .join("SETTINGS")
            .join("GCUSERSETTINGSDATA.MXML");
        if let Some(name) = std::fs::read_to_string(&settings)
            .ok()
            .and_then(|t| parse_settings(&t))
        {
            if let Some(code) = from_game_name(&name) {
                return (code.to_string(), "user settings".to_string());
            }
        }
        let manifest = root
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("appmanifest_275850.acf"))
            .unwrap_or_default();
        if let Some(name) = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|t| parse_manifest(&t))
        {
            if let Some(code) = from_game_name(&name) {
                return (code.to_string(), "steam".to_string());
            }
        }
    }
    ("en".to_string(), "fallback".to_string())
}

/// The inner `Language` value of the `TkLanguages` property.
fn parse_settings(xml: &str) -> Option<String> {
    let at = xml.find(r#"value="TkLanguages""#)?;
    let rest = &xml[at..];
    let end = rest.find("</Property>").unwrap_or(rest.len());
    let block = &rest[..end];
    let p = block.find(r#"name="Language""#)?;
    let tail = &block[p..];
    let v = tail.find(r#"value=""#)? + 7;
    let tail = &tail[v..];
    Some(tail[..tail.find('"')?].to_string())
}

/// The `language` value under `UserConfig` (else the first `language` key).
fn parse_manifest(acf: &str) -> Option<String> {
    let scan = |text: &str| -> Option<String> {
        for line in text.lines() {
            let mut q = line.split('"').filter(|s| !s.trim().is_empty());
            if q.next().map(|k| k.eq_ignore_ascii_case("language")) == Some(true) {
                if let Some(v) = q.next() {
                    return Some(v.to_string());
                }
            }
        }
        None
    };
    match acf.find("\"UserConfig\"") {
        Some(i) => scan(&acf[i..]).or_else(|| scan(acf)),
        None => scan(acf),
    }
}

/// Maps the game's `TkLanguages` names and Steam language names to
/// locale codes (`SUPPORTED`-style set shared with the strings work).
fn from_game_name(name: &str) -> Option<&'static str> {
    let n: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    Some(match n.as_str() {
        "english" | "usenglish" | "en" | "enus" | "engb" => "en",
        "french" | "fr" => "fr",
        "italian" | "it" => "it",
        "german" | "de" => "de",
        "spanish" | "es" => "es",
        "latinamericanspanish" | "latam" | "es419" | "esmx" => "es-419",
        "russian" | "ru" => "ru",
        "polish" | "pl" => "pl",
        "dutch" | "nl" => "nl",
        "portuguese" | "pt" | "ptpt" => "pt",
        "brazilianportuguese" | "brazilian" | "ptbr" => "pt-BR",
        "japanese" | "ja" | "jp" => "ja",
        "korean" | "koreana" | "ko" => "ko",
        "simplifiedchinese" | "schinese" | "tencentchinese" | "zh" | "zhcn" | "zhhans" => "zh",
        "traditionalchinese" | "tchinese" | "zhhant" | "zhtw" | "zhhk" => "zh-Hant",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map() {
        for (n, c) in [
            ("English", "en"),
            ("USEnglish", "en"),
            ("German", "de"),
            ("Dutch", "nl"),
            ("LatinAmericanSpanish", "es-419"),
            ("BrazilianPortuguese", "pt-BR"),
            ("SimplifiedChinese", "zh"),
            ("TencentChinese", "zh"),
            ("TraditionalChinese", "zh-Hant"),
            ("Korean", "ko"),
            ("nl", "nl"),
        ] {
            assert_eq!(from_game_name(n), Some(c), "{n}");
        }
        assert_eq!(from_game_name("Default"), None);
        assert_eq!(from_game_name("Klingon"), None);
    }

    #[test]
    fn settings_and_manifest_parse() {
        let xml = "<Property name=\"Language\" value=\"TkLanguages\">\n<Property name=\"Language\" value=\"German\" />\n</Property>";
        assert_eq!(parse_settings(xml).as_deref(), Some("German"));
        assert_eq!(parse_settings("<Data/>"), None);
        let acf = "\"UserConfig\"\n{\n\"language\"\t\"french\"\n}";
        assert_eq!(parse_manifest(acf).as_deref(), Some("french"));
        assert_eq!(parse_manifest("nothing here"), None);
    }

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
    fn chrome_dutch_and_fallback() {
        assert_eq!(chrome("nl", "on"), "aan");
        assert_eq!(chrome("nl", "off"), "uit");
        assert_eq!(chrome("nl", "select"), "selecteren");
        assert_eq!(chrome("nl", "adjust"), "aanpassen");
        assert_eq!(chrome("nl", "ok"), "ok");
        assert_eq!(chrome("nl", "reset"), "resetten");
        assert_eq!(chrome("nl", "close"), "sluiten");
        assert_eq!(chrome("xx", "on"), "on");
        assert_eq!(chrome("nl", "nope"), "");
    }

    #[test]
    fn chrome_covers_every_game_language() {
        // Every code the detector can produce must resolve all keys.
        for lang in [
            "en", "de", "fr", "it", "es", "es-419", "ru", "pl", "nl", "pt", "pt-BR", "ja", "zh",
            "zh-Hant", "ko",
        ] {
            for key in ["on", "off", "select", "adjust", "ok", "reset", "close"] {
                assert!(!chrome(lang, key).is_empty(), "{lang}/{key}");
            }
        }
    }
}
