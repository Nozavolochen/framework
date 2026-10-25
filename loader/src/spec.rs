//! Mod spec model: hand-parsed `mod.toml` (no dependencies,
//! predictable inside DllMain).
//!
//! ```toml
//! [mod]
//! id = "eu.savagecore.example_mod"
//! version = "0.1.0"
//! enabled = true
//!
//! [features]
//! first_patch = true
//!
//! [[patch]]
//! id = "example-patch"
//! feature = "first_patch"   # optional: gate on a [features] key
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

/// One `[[patch]]` entry.
#[derive(Debug, Clone)]
pub struct Patch {
    pub id: String,
    pub feature: Option<String>,
    pub op: Op,
}

/// One `mod.toml`.
#[derive(Debug, Clone)]
pub struct Mod {
    pub id: String,
    pub version: String,
    pub enabled: bool,
    /// (key, value) from `[features]`; missing keys default to true.
    pub features: Vec<(String, bool)>,
    pub patches: Vec<Patch>,
}

impl Mod {
    pub fn feature_on(&self, key: &str) -> bool {
        for (k, v) in &self.features {
            if k.eq_ignore_ascii_case(key) {
                return *v;
            }
        }
        true
    }
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
struct PatchBuilder {
    id: Option<String>,
    feature: Option<String>,
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
        Ok(Patch {
            id,
            feature: self.feature,
            op,
        })
    }
}

fn parse_num<T: std::str::FromStr>(v: &str) -> Option<T> {
    v.trim().parse().ok()
}

/// Parse one `mod.toml` document. Unknown keys are ignored so old
/// specs keep loading when new optional keys appear.
pub fn parse_mod(text: &str) -> Result<Mod, String> {
    let mut id = String::new();
    let mut version = String::new();
    let mut enabled = true;
    let mut features: Vec<(String, bool)> = Vec::new();
    let mut patches: Vec<Patch> = Vec::new();
    let mut cur: Option<PatchBuilder> = None;
    // None = outside, Some(false) = [mod], Some(true) = [features].
    let mut in_features = false;
    let mut in_mod = false;

    let mut flush = |cur: &mut Option<PatchBuilder>| -> Result<(), String> {
        if let Some(b) = cur.take() {
            patches.push(b.build()?);
        }
        Ok(())
    };

    for raw in text.lines() {
        let line = raw.split(['#', ';']).next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line.eq_ignore_ascii_case("[[patch]]") {
                flush(&mut cur)?;
                cur = Some(PatchBuilder::default());
                in_mod = false;
                continue;
            }
            flush(&mut cur)?;
            in_features = line.eq_ignore_ascii_case("[features]");
            in_mod = line.eq_ignore_ascii_case("[mod]");
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        if let Some(b) = cur.as_mut() {
            match key.as_str() {
                "id" => b.id = Some(unquote(v)),
                "feature" => b.feature = Some(unquote(v)),
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
                "enabled" => {
                    if let Some(b) = parse_bool(v) {
                        enabled = b;
                    }
                }
                _ => {}
            }
        } else if in_features {
            if let Some(b) = parse_bool(v) {
                features.push((key, b));
            }
        }
    }
    flush(&mut cur)?;
    if id.is_empty() {
        return Err("mod without [mod] id".into());
    }
    Ok(Mod {
        id,
        version,
        enabled,
        features,
        patches,
    })
}
