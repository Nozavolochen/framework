//! Mod spec model: hand-parsed `mod.toml` (no dependencies,
//! predictable inside DllMain).
//!
//! ```toml
//! [mod]
//! id = "eu.savagecore.example_mod"
//! version = "0.1.0"
//! enabled = true
//!
//! [[setting]]
//! id = "example-patch"   # matches a patch id: bool toggles it
//! kind = "bool"
//! label = "Apply the patch"
//! default = true
//!
//! [[patch]]
//! id = "example-patch"
//! op = "force-branch"
//! pattern = "48 3b df 0f 84 ?? ?? ?? ?? ..."
//! offset = 15
//! ```

/// A patch operation with its parameters.
#[derive(Debug, Clone)]
pub enum Op {
    /// Verify the `expect` prefix at `offset`, write `bytes`.
    WriteBytes {
        pat: Vec<u8>,
        mask: Vec<u8>,
        offset: usize,
        expect: Option<Vec<u8>>,
        bytes: Vec<u8>,
    },
    /// `0f 84 rel32` at `offset` becomes `e9 rel32' 90`, target kept.
    ForceBranch {
        pat: Vec<u8>,
        mask: Vec<u8>,
        offset: usize,
    },
    /// Many-hit pattern; keep the one backward jmp into a
    /// `test al,al` check, retarget to the check's skip destination.
    RetargetJmp {
        pat: Vec<u8>,
        mask: Vec<u8>,
        offset: usize,
    },
    /// String-anchored page-scale redirect; all-or-none with cave guards.
    PageSizeOverride { items: u32 },
    /// Single-hit hook on the slot renderer with vanilla-metric guards.
    DetourWithArgOverrides {
        pat: Vec<u8>,
        mask: Vec<u8>,
        slot_width: i32,
        cargo_rows: i32,
        tech_rows: i32,
        panel_width: f64,
    },
}

/// One translated label/description pair: a `[[mod.text]]` or
/// `[[setting.text]]` block. `lang` is a lowercase BCP 47 primary
/// tag (`nl`, `de`, `zh`). Empty label/description keeps the source
/// string for that field.
#[derive(Debug, Clone)]
pub struct TextEntry {
    pub lang: String,
    pub label: String,
    pub description: String,
}

/// One `[[setting]]` entry: a tunable the overlay can show and
/// the ops read live. `value` is the current choice; `default` is
/// used when `value` is absent.
#[derive(Debug, Clone)]
pub struct Setting {
    pub id: String,
    pub kind: SettingKind,
    pub label: String,
    /// Explainer line under the row; empty hides it.
    pub description: String,
    pub default: String,
    pub value: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub choices: Vec<String>,
    pub texts: Vec<TextEntry>,
}

impl Setting {
    /// Live value: the saved `value`, or `default` when absent.
    pub fn current(&self) -> &str {
        self.value.as_deref().unwrap_or(&self.default)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Enum,
    Float,
    Int,
    Text,
}

impl SettingKind {
    fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "bool" => Some(Self::Bool),
            "enum" => Some(Self::Enum),
            "float" => Some(Self::Float),
            "int" => Some(Self::Int),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

/// One `[[patch]]` entry.
#[derive(Debug, Clone)]
pub struct Patch {
    pub id: String,
    pub op: Op,
}

/// One `mod.toml`.
#[derive(Debug, Clone)]
pub struct Mod {
    pub id: String,
    pub version: String,
    /// Display name for the overlay; defaults to the id.
    pub label: String,
    /// One-line byline shown under the group header; empty hides it.
    pub description: String,
    pub enabled: bool,
    pub settings: Vec<Setting>,
    pub patches: Vec<Patch>,
    pub texts: Vec<TextEntry>,
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Decode `"48 3b df ?? ..."` into (bytes, mask). `??` or `?` is a
/// wildcard; anything else must be two hex digits.
pub fn decode_pattern(s: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut pat = Vec::new();
    let mut mask = Vec::new();
    for tok in s.split_whitespace() {
        if tok == "??" || tok == "?" {
            pat.push(0);
            mask.push(0);
        } else if tok.len() == 2 && tok.bytes().all(|b| b.is_ascii_hexdigit()) {
            pat.push(u8::from_str_radix(tok, 16).map_err(|e| e.to_string())?);
            mask.push(1);
        } else {
            return Err(format!("bad pattern token {tok:?}"));
        }
    }
    if pat.is_empty() {
        return Err("empty pattern".into());
    }
    Ok((pat, mask))
}

/// Decode a plain hex byte string (`"66 0f 1f 44 00 00"`, no wildcards).
pub fn decode_bytes(s: &str) -> Result<Vec<u8>, String> {
    let (pat, mask) = decode_pattern(s)?;
    if mask.iter().any(|&m| m == 0) {
        return Err("wildcards not allowed here".into());
    }
    Ok(pat)
}

#[derive(Default)]
struct TextBuilder {
    lang: Option<String>,
    label: Option<String>,
    description: Option<String>,
}

impl TextBuilder {
    fn build(self, owner: &str) -> Result<TextEntry, String> {
        let lang = self.lang.ok_or(format!("{owner}: text without lang"))?;
        let lang = lang.trim().to_lowercase();
        if lang.is_empty() {
            return Err(format!("{owner}: text without lang"));
        }
        Ok(TextEntry {
            lang,
            label: self.label.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
        })
    }
}

#[derive(Default)]
struct SettingBuilder {
    id: Option<String>,
    kind: Option<String>,
    label: Option<String>,
    description: Option<String>,
    default: Option<String>,
    value: Option<String>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
    choices: Vec<String>,
    texts: Vec<TextBuilder>,
}

impl SettingBuilder {
    fn build(self) -> Result<Setting, String> {
        let id = self.id.ok_or("setting without id")?;
        let kind_key = self.kind.ok_or(format!("setting {id}: without kind"))?;
        let kind = SettingKind::from_key(&kind_key).ok_or(format!(
            "setting {id}: unknown kind {kind_key:?} (bool, enum, float, int, text)"
        ))?;
        if kind == SettingKind::Enum && self.choices.is_empty() {
            return Err(format!("setting {id}: enum without choices"));
        }
        let default = self
            .default
            .ok_or(format!("setting {id}: without default"))?;
        let mut texts = Vec::with_capacity(self.texts.len());
        for t in self.texts {
            texts.push(t.build(&format!("setting {id}"))?);
        }
        Ok(Setting {
            id: id.clone(),
            kind,
            label: self.label.unwrap_or(id),
            description: self.description.unwrap_or_default(),
            default,
            value: self.value,
            min: self.min,
            max: self.max,
            step: self.step,
            choices: self.choices,
            texts,
        })
    }
}

fn parse_choices(v: &str) -> Vec<String> {
    let t = v.trim();
    let inner = t
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(t);
    inner
        .split(',')
        .map(|c| unquote(c).trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

#[derive(Default)]
struct PatchBuilder {
    id: Option<String>,
    op: Option<String>,
    pattern: Option<String>,
    offset: Option<usize>,
    expect: Option<String>,
    bytes: Option<String>,
    items: Option<u32>,
    slot_width: Option<i32>,
    cargo_rows: Option<i32>,
    tech_rows: Option<i32>,
    panel_width: Option<f64>,
}

impl PatchBuilder {
    fn build(self) -> Result<Patch, String> {
        let id = self.id.ok_or("patch without id")?;
        let op_name = self.op.ok_or(format!("patch {id}: without op"))?;
        let op = match op_name.as_str() {
            "write-bytes" => {
                let (pat, mask) = decode_pattern(
                    &self
                        .pattern
                        .ok_or(format!("patch {id}: write-bytes without pattern"))?,
                )?;
                let bytes = decode_bytes(
                    &self
                        .bytes
                        .ok_or(format!("patch {id}: write-bytes without bytes"))?,
                )?;
                let expect = self.expect.as_deref().map(decode_bytes).transpose()?;
                if let Some(e) = &expect {
                    if e.len() > bytes.len() {
                        return Err(format!("patch {id}: expect longer than bytes"));
                    }
                }
                Op::WriteBytes {
                    pat,
                    mask,
                    offset: self.offset.unwrap_or(0),
                    expect,
                    bytes,
                }
            }
            "force-branch" => {
                let (pat, mask) = decode_pattern(
                    &self
                        .pattern
                        .ok_or(format!("patch {id}: force-branch without pattern"))?,
                )?;
                Op::ForceBranch {
                    pat,
                    mask,
                    offset: self
                        .offset
                        .ok_or(format!("patch {id}: force-branch without offset"))?,
                }
            }
            "retarget-jmp" => {
                let (pat, mask) = decode_pattern(
                    &self
                        .pattern
                        .ok_or(format!("patch {id}: retarget-jmp without pattern"))?,
                )?;
                Op::RetargetJmp {
                    pat,
                    mask,
                    offset: self
                        .offset
                        .ok_or(format!("patch {id}: retarget-jmp without offset"))?,
                }
            }
            "page-size-override" => Op::PageSizeOverride {
                items: self
                    .items
                    .ok_or(format!("patch {id}: page-size-override without items"))?,
            },
            "detour-with-arg-overrides" => {
                let (pat, mask) = decode_pattern(&self.pattern.ok_or(format!(
                    "patch {id}: detour-with-arg-overrides without pattern"
                ))?)?;
                Op::DetourWithArgOverrides {
                    pat,
                    mask,
                    slot_width: self.slot_width.unwrap_or(63),
                    cargo_rows: self.cargo_rows.unwrap_or(7),
                    tech_rows: self.tech_rows.unwrap_or(4),
                    panel_width: self.panel_width.unwrap_or(724.5),
                }
            }
            other => return Err(format!("patch {id}: unknown op {other:?}")),
        };
        Ok(Patch { id, op })
    }
}

fn parse_num<T: std::str::FromStr>(v: &str) -> Option<T> {
    v.trim().parse().ok()
}

/// Strip `#`/`;` comments, keeping quoted text intact. A naive
/// split would chop values like `"a; b"` mid-string.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && (c == '#' || c == ';') {
            return line[..i].trim_end();
        }
    }
    line
}

/// Parse one `mod.toml` document. Unknown keys are ignored so old
/// specs keep loading when new optional keys appear.
pub fn parse_mod(text: &str) -> Result<Mod, String> {
    let mut id = String::new();
    let mut version = String::new();
    let mut label = String::new();
    let mut description = String::new();
    let mut enabled = true;
    let mut settings: Vec<Setting> = Vec::new();
    let mut patches: Vec<Patch> = Vec::new();
    let mut cur: Option<PatchBuilder> = None;
    let mut cur_setting: Option<SettingBuilder> = None;
    let mut cur_text: Option<TextBuilder> = None;
    // Whether a pending [[mod.text]] / [[setting.text]] block belongs
    // to the mod (true) or the current setting (false).
    let mut text_is_mod = false;
    let mut mod_texts: Vec<TextBuilder> = Vec::new();
    // Outside, [mod], or inside a [[patch]] / [[setting]] block.
    // Any other section header is ignored, with its keys.
    let mut in_mod = false;

    let mut flush = |cur: &mut Option<PatchBuilder>| -> Result<(), String> {
        if let Some(b) = cur.take() {
            patches.push(b.build()?);
        }
        Ok(())
    };

    let mut flush_setting = |cur: &mut Option<SettingBuilder>| -> Result<(), String> {
        if let Some(b) = cur.take() {
            settings.push(b.build()?);
        }
        Ok(())
    };

    // Move a pending text block into its parent builder / list.
    let flush_text = |cur_setting: &mut Option<SettingBuilder>,
                      mod_texts: &mut Vec<TextBuilder>,
                      cur_text: &mut Option<TextBuilder>,
                      text_is_mod: &mut bool|
     -> Result<(), String> {
        if let Some(t) = cur_text.take() {
            if *text_is_mod {
                mod_texts.push(t);
            } else {
                match cur_setting.as_mut() {
                    Some(s) => s.texts.push(t),
                    None => return Err("text outside [[mod]] or [[setting]]".into()),
                }
            }
        }
        Ok(())
    };

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line.eq_ignore_ascii_case("[[patch]]") {
                flush_text(
                    &mut cur_setting,
                    &mut mod_texts,
                    &mut cur_text,
                    &mut text_is_mod,
                )?;
                flush(&mut cur)?;
                flush_setting(&mut cur_setting)?;
                cur = Some(PatchBuilder::default());
                in_mod = false;
                continue;
            }
            if line.eq_ignore_ascii_case("[[setting]]") {
                flush_text(
                    &mut cur_setting,
                    &mut mod_texts,
                    &mut cur_text,
                    &mut text_is_mod,
                )?;
                flush(&mut cur)?;
                flush_setting(&mut cur_setting)?;
                cur_setting = Some(SettingBuilder::default());
                in_mod = false;
                continue;
            }
            if line.eq_ignore_ascii_case("[[mod.text]]") {
                flush_text(
                    &mut cur_setting,
                    &mut mod_texts,
                    &mut cur_text,
                    &mut text_is_mod,
                )?;
                cur_text = Some(TextBuilder::default());
                text_is_mod = true;
                in_mod = false;
                continue;
            }
            if line.eq_ignore_ascii_case("[[setting.text]]") {
                flush_text(
                    &mut cur_setting,
                    &mut mod_texts,
                    &mut cur_text,
                    &mut text_is_mod,
                )?;
                if cur_setting.is_none() {
                    return Err("text outside [[setting]]".into());
                }
                cur_text = Some(TextBuilder::default());
                text_is_mod = false;
                in_mod = false;
                continue;
            }
            flush_text(
                &mut cur_setting,
                &mut mod_texts,
                &mut cur_text,
                &mut text_is_mod,
            )?;
            flush(&mut cur)?;
            flush_setting(&mut cur_setting)?;
            in_mod = line.eq_ignore_ascii_case("[mod]");
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        if let Some(t) = cur_text.as_mut() {
            match key.as_str() {
                "lang" => t.lang = Some(unquote(v)),
                "label" => t.label = Some(unquote(v)),
                "description" => t.description = Some(unquote(v)),
                _ => {}
            }
            continue;
        }
        if let Some(s) = cur_setting.as_mut() {
            match key.as_str() {
                "id" => s.id = Some(unquote(v)),
                "kind" => s.kind = Some(unquote(v)),
                "label" => s.label = Some(unquote(v)),
                "description" => s.description = Some(unquote(v)),
                "default" => s.default = Some(unquote(v)),
                "value" => s.value = Some(unquote(v)),
                "min" => s.min = parse_num(v),
                "max" => s.max = parse_num(v),
                "step" => s.step = parse_num(v),
                "choices" => s.choices = parse_choices(v),
                _ => {}
            }
            continue;
        }
        if let Some(b) = cur.as_mut() {
            match key.as_str() {
                "id" => b.id = Some(unquote(v)),
                "op" => b.op = Some(unquote(v)),
                "pattern" => b.pattern = Some(unquote(v)),
                "offset" => b.offset = parse_num(v),
                "expect" => b.expect = Some(unquote(v)),
                "bytes" => b.bytes = Some(unquote(v)),
                "items" => b.items = parse_num(v),
                "slot_width" => b.slot_width = parse_num(v),
                "cargo_rows" => b.cargo_rows = parse_num(v),
                "tech_rows" => b.tech_rows = parse_num(v),
                "panel_width" => b.panel_width = parse_num(v),
                _ => {}
            }
            continue;
        }
        if in_mod {
            match key.as_str() {
                "id" => id = unquote(v),
                "version" => version = unquote(v),
                "label" => label = unquote(v),
                "description" => description = unquote(v),
                "enabled" => {
                    if let Some(b) = parse_bool(v) {
                        enabled = b;
                    }
                }
                _ => {}
            }
        }
    }
    flush_text(
        &mut cur_setting,
        &mut mod_texts,
        &mut cur_text,
        &mut text_is_mod,
    )?;
    flush(&mut cur)?;
    flush_setting(&mut cur_setting)?;
    if id.is_empty() {
        return Err("mod without [mod] id".into());
    }
    let mut texts = Vec::with_capacity(mod_texts.len());
    for t in mod_texts {
        texts.push(t.build("mod")?);
    }
    Ok(Mod {
        id: id.clone(),
        version,
        label: if label.is_empty() { id } else { label },
        description,
        enabled,
        settings,
        patches,
        texts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_TOML: &str = r#"
[mod]
id = "eu.savagecore.example_mod"
version = "0.0.0"
label = "Example Mod"
enabled = true

[[setting]]
id = "mode"
kind = "enum"
label = "Mode"
choices = ["volume", "area", "linear", "off"]
default = "volume"
value = "area"

[[setting]]
id = "factor"
kind = "float"
label = "Factor"
default = 1.0
min = 0.25
max = 4.0
step = 0.25
value = 2.0

[[setting]]
id = "count"
kind = "int"
label = "Count"
default = 64
min = 1
max = 512
value = 128

[[patch]]
id = "example-patch"
op = "write-bytes"
pattern = "48 3b df 0f 84 ?? ?? ?? ??"
offset = 15
expect = "0f 84"
bytes = "66 0f 1f 44 00 00"
"#;

    fn setting<'a>(m: &'a Mod, id: &str) -> &'a Setting {
        m.settings.iter().find(|s| s.id == id).unwrap()
    }

    #[test]
    fn mod_label_parsed_with_id_fallback() {
        let m = parse_mod(EXAMPLE_TOML).unwrap();
        assert_eq!(m.label, "Example Mod");
        let toml = EXAMPLE_TOML.replace("label = \"Example Mod\"\n", "");
        let m = parse_mod(&toml).unwrap();
        assert_eq!(m.label, m.id);
    }

    #[test]
    fn mod_description_parsed_empty_by_default() {
        let m = parse_mod(EXAMPLE_TOML).unwrap();
        assert_eq!(m.description, "");
        let toml = EXAMPLE_TOML.replace(
            "label = \"Example Mod\"\n",
            "label = \"Example Mod\"\ndescription = \"Test byline.\"\n",
        );
        let m = parse_mod(&toml).unwrap();
        assert_eq!(m.description, "Test byline.");
    }

    #[test]
    fn setting_description_parsed_empty_by_default() {
        let m = parse_mod(EXAMPLE_TOML).unwrap();
        assert!(m.settings.iter().all(|s| s.description.is_empty()));
        let toml = EXAMPLE_TOML.replace(
            "label = \"Mode\"\n",
            "label = \"Mode\"\ndescription = \"How it scales.\"\n",
        );
        let m = parse_mod(&toml).unwrap();
        assert_eq!(setting(&m, "mode").description, "How it scales.");
    }

    #[test]
    fn semicolons_inside_quotes_survive() {
        let toml = "[mod]\nid = \"x\"\n[other]\n[[setting]]\nid = \"s\"\nkind = \"text\"\nlabel = \"L\"\ndescription = \"a; b # c\"\ndefault = \"d\"\n";
        let m = parse_mod(toml).unwrap();
        assert_eq!(m.settings[0].description, "a; b # c");
        // A real trailing comment still strips; unknown sections are ignored.
        let toml = "[mod]\nid = \"x\" # game mod\n[features]\nold_flag = true\n";
        let m = parse_mod(toml).unwrap();
        assert_eq!(m.id, "x");
    }

    #[test]
    fn write_bytes_parses() {
        let m = parse_mod(EXAMPLE_TOML).unwrap();
        assert_eq!(m.id, "eu.savagecore.example_mod");
        assert!(m.enabled);
        assert_eq!(m.patches.len(), 1);
        let p = &m.patches[0];
        assert_eq!(p.id, "example-patch");
        assert!(matches!(p.op, Op::WriteBytes { .. }));
        assert_eq!(m.settings.len(), 3);
        let md = setting(&m, "mode");
        assert_eq!(md.kind, SettingKind::Enum);
        assert_eq!(md.choices, vec!["volume", "area", "linear", "off"]);
        assert_eq!(md.default, "volume");
        assert_eq!(md.value.as_deref(), Some("area"));
        let fc = setting(&m, "factor");
        assert_eq!(fc.kind, SettingKind::Float);
        assert_eq!(fc.value.as_deref(), Some("2.0"));
        assert_eq!(fc.min, Some(0.25));
        assert_eq!(fc.max, Some(4.0));
        assert_eq!(fc.step, Some(0.25));
        let ct = setting(&m, "count");
        assert_eq!(ct.kind, SettingKind::Int);
        assert_eq!(ct.value.as_deref(), Some("128"));
    }

    #[test]
    fn settings_reject_bad_schema() {
        // Unknown kind.
        let toml = EXAMPLE_TOML.replace("kind = \"enum\"", "kind = \"slider\"");
        assert!(parse_mod(&toml).is_err());
        // Enum without choices.
        let toml = EXAMPLE_TOML.replace(
            "choices = [\"volume\", \"area\", \"linear\", \"off\"]",
            "choices = []",
        );
        assert!(parse_mod(&toml).is_err());
        // Missing id.
        let toml = EXAMPLE_TOML.replace("id = \"mode\"\n", "");
        assert!(parse_mod(&toml).is_err());
        // Missing kind.
        let toml = EXAMPLE_TOML.replace("kind = \"float\"\n", "");
        assert!(parse_mod(&toml).is_err());
        // Missing default.
        let toml = EXAMPLE_TOML.replace("default = 64\n", "");
        assert!(parse_mod(&toml).is_err());
    }

    #[test]
    fn write_bytes_needs_pattern_and_bytes() {
        for key in ["pattern", "bytes"] {
            let toml = EXAMPLE_TOML.replace(&format!("{key} = "), "# ");
            assert!(parse_mod(&toml).is_err(), "accepted without {key}");
        }
    }

    #[test]
    fn unknown_op_still_rejected() {
        let toml = EXAMPLE_TOML.replace("write-bytes", "no-such-op");
        assert!(parse_mod(&toml).is_err());
    }

    const TEXT_TOML: &str = r#"
[mod]
id = "eu.savagecore.text_mod"
label = "Source Label"
description = "Source byline."

[[mod.text]]
lang = "nl"
label = "Bronlabel"
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
lang = "de"
label = "Quellzeile"
"#;

    #[test]
    fn texts_parse_with_lang_lowercased() {
        let m = parse_mod(TEXT_TOML).unwrap();
        assert_eq!(m.texts.len(), 1);
        assert_eq!(m.texts[0].lang, "nl");
        assert_eq!(m.texts[0].label, "Bronlabel");
        let s = setting(&m, "feat");
        assert_eq!(s.texts.len(), 2);
        assert_eq!(s.texts[0].lang, "nl");
        assert_eq!(s.texts[1].lang, "de");
        // German entry has no description: keeps source on apply.
        assert!(s.texts[1].description.is_empty());
    }

    #[test]
    fn texts_reject_missing_lang() {
        let toml = TEXT_TOML.replace("lang = \"nl\"\n", "");
        assert!(parse_mod(&toml).is_err());
    }

    #[test]
    fn setting_text_outside_setting_rejected() {
        let toml = "[[setting.text]]\nlang = \"nl\"\n";
        assert!(parse_mod(&toml).is_err());
    }

    #[test]
    fn patch_after_texts_still_parses() {
        // A patch after text sections must not swallow their keys.
        let toml = format!("{TEXT_TOML}\n[[patch]]\nid = \"p\"\nop = \"force-branch\"\npattern = \"48 3b df\"\noffset = 1\n");
        let m = parse_mod(&toml).unwrap();
        assert_eq!(m.patches.len(), 1);
    }
}
