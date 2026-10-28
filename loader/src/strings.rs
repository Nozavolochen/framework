//! Localised strings for the overlay chrome and future mods.
//!
//! Source of truth is `loader/strings/<code>.json`: one flat object per
//! language, every file with the same keys as `en.json`. English is built
//! in and is the fallback for missing keys and unknown languages.
//! `loader/strings` is the Weblate component root: translators edit the
//! JSON there, never Rust. A new language ships by adding its file plus
//! one line in [`EXTRA`] below.
//!
//! The parser is hand-rolled (flat string maps only) to keep the loader
//! dependency-free and predictable inside DllMain. `check_strings.py`
//! enforces the same shape from the outside.

use std::path::Path;

/// Language codes the game can report (same set as NMS.rs).
pub const SUPPORTED: &[&str] = &[
    "en", "de", "fr", "it", "es", "es-419", "ru", "pl", "nl", "pt", "pt-BR", "ja", "zh", "zh-Hant",
    "ko",
];

const EN_JSON: &str = include_str!("../strings/en.json");

/// Shipped translations beyond English: one entry per file Weblate has
/// filled in. Empty until the first language lands.
const EXTRA: &[(&str, &str)] = &[];

fn english_table() -> Vec<(String, String)> {
    parse(EN_JSON).expect("en.json is built in and must parse")
}

fn extra_table(lang: &str) -> Option<Vec<(String, String)>> {
    EXTRA
        .iter()
        .find(|(code, _)| *code == lang)
        .map(|(_, json)| parse(json).expect("shipped strings must parse"))
}

/// One language worth of strings: the best match plus English underneath.
/// Missing keys and unknown languages behave as English.
#[derive(Debug, Clone, Default)]
pub struct Strings {
    lang: String,
    active: Vec<(String, String)>,
    en: Vec<(String, String)>,
}

impl Strings {
    /// Load `lang` (plus `extra_json`, e.g. a mod's own table, checked
    /// first). Unknown codes behave as English.
    pub fn load(lang: &str, extra_json: Option<&str>) -> Self {
        let code = if SUPPORTED.contains(&lang) {
            lang
        } else {
            "en"
        };
        let mut active = extra_json.and_then(|j| parse(j).ok()).unwrap_or_default();
        if code != "en" {
            if let Some(t) = extra_table(code) {
                active.extend(t);
            }
        }
        Self {
            lang: code.to_string(),
            active,
            en: english_table(),
        }
    }

    /// The code that was requested.
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// One string: the best match, else English, else `""`.
    pub fn get(&self, key: &str) -> &str {
        self.active
            .iter()
            .find(|(k, _)| k == key)
            .or_else(|| self.en.iter().find(|(k, _)| k == key))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

/// Parse a flat JSON string map (`{"key": "value", ...}`). Only what
/// translators can produce is accepted: objects, string keys and values,
/// `"` `\\` `/` `b` `f` `n` `r` `t` and `uXXXX` escapes. Anything else is
/// an error naming the byte offset.
pub fn parse(json: &str) -> Result<Vec<(String, String)>, String> {
    let b = json.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    skip_ws(b, &mut i);
    expect(b, &mut i, b'{')?;
    skip_ws(b, &mut i);
    if peek(b, i) == Some(b'}') {
        i += 1;
        skip_ws(b, &mut i);
        if i != b.len() {
            return Err(format!("trailing text at byte {i}"));
        }
        return Ok(out);
    }
    loop {
        skip_ws(b, &mut i);
        let key = parse_string(b, &mut i)?;
        skip_ws(b, &mut i);
        expect(b, &mut i, b':')?;
        skip_ws(b, &mut i);
        let value = parse_string(b, &mut i)?;
        out.push((key, value));
        skip_ws(b, &mut i);
        match peek(b, i) {
            Some(b',') => {
                i += 1;
            }
            Some(b'}') => {
                i += 1;
                break;
            }
            _ => return Err(format!("want ',' or '}}' at byte {i}")),
        }
    }
    skip_ws(b, &mut i);
    if i != b.len() {
        return Err(format!("trailing text at byte {i}"));
    }
    Ok(out)
}

fn peek(b: &[u8], i: usize) -> Option<u8> {
    b.get(i).copied()
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while matches!(peek(b, *i), Some(c) if c.is_ascii_whitespace()) {
        *i += 1;
    }
}

fn expect(b: &[u8], i: &mut usize, want: u8) -> Result<(), String> {
    if peek(b, *i) == Some(want) {
        *i += 1;
        Ok(())
    } else {
        Err(format!("want '{}' at byte {i}", want as char))
    }
}

fn parse_string(b: &[u8], i: &mut usize) -> Result<String, String> {
    expect(b, i, b'"')?;
    let mut s = String::new();
    loop {
        let c = peek(b, *i).ok_or_else(|| format!("unterminated string at byte {i}"))?;
        *i += 1;
        match c {
            b'"' => return Ok(s),
            b'\\' => {
                let e = peek(b, *i).ok_or_else(|| format!("bad escape at byte {i}"))?;
                *i += 1;
                match e {
                    b'"' => s.push('"'),
                    b'\\' => s.push('\\'),
                    b'/' => s.push('/'),
                    b'b' => s.push('\u{0008}'),
                    b'f' => s.push('\u{000C}'),
                    b'n' => s.push('\n'),
                    b'r' => s.push('\r'),
                    b't' => s.push('\t'),
                    b'u' => s.push(parse_unicode(b, i)?),
                    _ => return Err(format!("bad escape at byte {i}")),
                }
            }
            0x00..=0x1F => return Err(format!("control character at byte {i}")),
            _ => {
                // Copy the full UTF-8 sequence starting here.
                let rest = &b[*i - 1..];
                let text =
                    std::str::from_utf8(rest).map_err(|_| format!("bad UTF-8 at byte {i}"))?;
                let ch = text
                    .chars()
                    .next()
                    .ok_or_else(|| format!("bad UTF-8 at byte {i}"))?;
                s.push(ch);
                *i += ch.len_utf8() - 1;
            }
        }
    }
}

fn hex4(b: &[u8], i: &mut usize) -> Result<u32, String> {
    if *i + 4 > b.len() {
        return Err(format!("bad unicode escape at byte {i}"));
    }
    let mut v = 0u32;
    for k in 0..4 {
        let d = match b[*i + k] {
            c @ b'0'..=b'9' => (c - b'0') as u32,
            c @ b'a'..=b'f' => (c - b'a' + 10) as u32,
            c @ b'A'..=b'F' => (c - b'A' + 10) as u32,
            _ => return Err(format!("bad unicode escape at byte {i}")),
        };
        v = v * 16 + d;
    }
    *i += 4;
    Ok(v)
}

fn parse_unicode(b: &[u8], i: &mut usize) -> Result<char, String> {
    let v = hex4(b, i)?;
    let v = if (0xD800..0xDC00).contains(&v) {
        // High surrogate: a `\uDC00`-`\uDFFF` low half must follow.
        if peek(b, *i) == Some(b'\\') && b.get(*i + 1) == Some(&b'u') {
            *i += 2;
            let lo = hex4(b, i)?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(format!("bad surrogate pair at byte {i}"));
            }
            0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            return Err(format!("lone surrogate at byte {i}"));
        }
    } else {
        v
    };
    char::from_u32(v).ok_or_else(|| format!("bad unicode escape at byte {i}"))
}

/// UTF-8 text in a zero-filled buffer for the game, cut at a character
/// boundary, always NUL-terminated (at most `N - 1` text bytes). For
/// mod in-game notices (none in the loader yet); kept with the strings.
#[allow(dead_code)]
pub fn fixed<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut end = text.len().min(N.saturating_sub(1));
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].copy_from_slice(&text.as_bytes()[..end]);
    out
}

// ---- language detection (file-based, no game memory reads) ------------------
// Ported from NMS.rs (`common::lang`): the game rewrites
// `Binaries\SETTINGS\GCUSERSETTINGSDATA.MXML` itself, so a read sees the
// current choice. `Default` means "follow Steam", so fall back to
// `language` in `steamapps\appmanifest_275850.acf`, then English.

const USER_SETTINGS: &str = "GCUSERSETTINGSDATA.MXML";
const APP_MANIFEST: &str = "appmanifest_275850.acf";

/// The UI language as a locale code for `root` (the game folder holding
/// `Binaries`): `(&'static str code, &str source)`, English on fallback.
pub fn detect_in(root: &Path) -> (&'static str, &'static str) {
    let settings = root.join("Binaries").join("SETTINGS").join(USER_SETTINGS);
    if let Some(name) = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|t| parse_settings(&t))
    {
        if !name.eq_ignore_ascii_case("default") {
            if let Some(code) = from_game_name(&name) {
                return (code, "user settings");
            }
        }
    }
    let manifest = root
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(APP_MANIFEST))
        .unwrap_or_default();
    if let Some(name) = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|t| parse_manifest(&t))
    {
        if let Some(code) = from_game_name(&name) {
            return (code, "steam");
        }
    }
    ("en", "fallback")
}

/// The inner `Language` value of the `TkLanguages` property.
pub fn parse_settings(xml: &str) -> Option<String> {
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
pub fn parse_manifest(acf: &str) -> Option<String> {
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

/// Maps the game's `TkLanguages` names and Steam language names to codes.
pub fn from_game_name(name: &str) -> Option<&'static str> {
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

    const KEYS: &[&str] = &[
        "chrome.on",
        "chrome.off",
        "chrome.select",
        "chrome.adjust",
        "chrome.ok",
        "chrome.reset",
        "chrome.close",
    ];

    #[test]
    fn english_has_every_chrome_key() {
        let t = Strings::load("en", None);
        for k in KEYS {
            assert!(!t.get(k).is_empty(), "{k}");
        }
    }

    #[test]
    fn fallback_is_english() {
        let t = Strings::load("de", None);
        assert_eq!(t.lang(), "de");
        assert_eq!(t.get("chrome.on"), "on");
        assert_eq!(Strings::load("xx", None).lang(), "en");
        assert_eq!(Strings::load("", None).lang(), "en");
        let t = Strings::load("xx", None);
        assert_eq!(t.get("chrome.close"), "close");
        assert_eq!(Strings::load("", None).get("chrome.ok"), "ok");
        assert_eq!(Strings::load("en", None).get("no.such.key"), "");
    }

    #[test]
    fn extra_json_wins_over_english() {
        let t = Strings::load("en", Some(r#"{"chrome.on": "an"}"#));
        assert_eq!(t.get("chrome.on"), "an");
        assert_eq!(t.get("chrome.off"), "off");
    }

    #[test]
    fn rejects_non_flat_json() {
        assert!(parse(r#"{"a": "b"}"#).is_ok());
        assert!(parse("{}").unwrap().is_empty());
        assert!(parse(r#"{"a": 1}"#).is_err());
        assert!(parse(r#"["a"]"#).is_err());
        assert!(parse(r#"{"a": "b",}"#).is_err());
        assert!(parse(r#"{"a": "b"} trailing"#).is_err());
        assert!(parse("{\"a\": \"\\ud83d\\ude00\"}").unwrap()[0].1 == "\u{1F600}");
    }

    #[test]
    fn fixed_buffers_cut_on_char_boundaries() {
        let short: [u8; 8] = fixed("abcdefghij");
        assert_eq!(&short, b"abcdefg\0");
        let cut: [u8; 4] = fixed("abé");
        assert_eq!(&cut, b"ab\0\0");
    }

    #[test]
    fn names_map() {
        for (n, c) in [
            ("English", "en"),
            ("German", "de"),
            ("LatinAmericanSpanish", "es-419"),
            ("BrazilianPortuguese", "pt-BR"),
            ("SimplifiedChinese", "zh"),
            ("TraditionalChinese", "zh-Hant"),
            ("Korean", "ko"),
        ] {
            assert_eq!(from_game_name(n), Some(c), "{n}");
        }
        assert_eq!(from_game_name("Default"), None);
    }

    #[test]
    fn settings_and_manifest_parse() {
        let xml = "<Property name=\"Language\" value=\"TkLanguages\">\n<Property name=\"Language\" value=\"German\" />\n</Property>";
        assert_eq!(parse_settings(xml).as_deref(), Some("German"));
        assert_eq!(parse_settings("<Data/>"), None);
        let acf = "\"UserConfig\"\n{\n\"language\"\t\"french\"\n}";
        assert_eq!(parse_manifest(acf).as_deref(), Some("french"));
    }
}
