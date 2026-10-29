//! File-image planning for the popup page-size patch.
//!
//! The game keeps its Transfer/Craft page size (8 items) in code: a 0.125
//! page-scale float consumed via RIP-relative movss loads, plus next/prev
//! page step bytes in the slot filler. The algorithm (ported from the
//! GPL-3.0 NMS.rs bigger_item_grids sigscan):
//! 1. find string anchors (TRANSFER_RECEIVE_DESC, PRODUCTS_ALL, SLOT%02d),
//! 2. locate referencing functions via RIP-relative LEA scan,
//! 3. bound each function via CC padding,
//! 4. find 0.125 loads inside, redirect them to a code cave (function-tail
//!    CC padding) holding 1/items,
//! 5. rewrite the next/prev page steps to items.
//! All-or-none: every site must verify before anything is written.

use std::ops::Range;

use crate::spec::XrefPolicy;

pub struct Section {
    pub name: [u8; 8],
    pub rva: usize,
    pub raw_ptr: usize,
    pub raw_size: usize,
    pub exec: bool,
}

pub struct FileImage {
    pub bytes: Vec<u8>,
    pub sections: Vec<Section>,
}

fn u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn i32le(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl FileImage {
    pub fn load(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if bytes.len() < 0x44 || u16(&bytes, 0) != 0x5A4D {
            return Err("not a PE image".into());
        }
        let lf = u32(&bytes, 0x3C) as usize;
        let nsec = u16(&bytes, lf + 6) as usize;
        if u16(&bytes, lf + 24) != 0x20B {
            return Err("not a 64-bit image".into());
        }
        let mut sections = Vec::new();
        for i in 0..nsec {
            let o = lf + 24 + 240 + i * 40;
            if o + 40 > bytes.len() {
                return Err("truncated section table".into());
            }
            let mut name = [0u8; 8];
            name.copy_from_slice(&bytes[o..o + 8]);
            let raw_size = u32(&bytes, o + 16) as usize;
            let rva = u32(&bytes, o + 12) as usize;
            let raw_ptr = u32(&bytes, o + 20) as usize;
            let chars = u32(&bytes, o + 36);
            sections.push(Section {
                name,
                rva,
                raw_ptr,
                raw_size,
                exec: chars & 0x20000000 != 0,
            });
        }
        Ok(Self { bytes, sections })
    }

    fn off(&self, rva: usize) -> Option<usize> {
        for s in &self.sections {
            if rva >= s.rva && rva - s.rva < s.raw_size {
                return Some(s.raw_ptr + (rva - s.rva));
            }
        }
        None
    }

    pub fn get(&self, rva: usize, len: usize) -> Option<&[u8]> {
        let o = self.off(rva)?;
        self.bytes.get(o..o + len)
    }

    pub fn exec_ranges(&self) -> Vec<Range<usize>> {
        // (rva_start, rva_end) of executable sections with raw data.
        let mut out = Vec::new();
        for s in &self.sections {
            if s.exec && s.raw_size != 0 {
                out.push(s.rva..s.rva + s.raw_size);
            }
        }
        out
    }

    pub fn text_range(&self) -> Option<(usize, usize)> {
        for s in &self.sections {
            if s.name == *b".text\0\0\0" && s.exec && s.raw_size != 0 {
                return Some((s.rva, s.rva + s.raw_size));
            }
        }
        None
    }
}

const PAGE_SCALE: f32 = 0.125;
const RIP_LEA_RM: [u8; 8] = [0x05, 0x0D, 0x15, 0x1D, 0x25, 0x2D, 0x35, 0x3D];
const NEXT_PAGE: [u8; 11] = [
    0xC1, 0xE1, 0x03, 0x83, 0xC0, 0x08, 0x3B, 0xC8, 0x0F, 0x4C, 0xC1,
];
const PREV_PAGE: [u8; 3] = [0x83, 0xC1, 0xF8];

pub struct ScaleRedirect {
    pub cave_rva: usize,
    pub scale: f32,
    pub patches: Vec<(usize, Vec<u8>, Vec<u8>)>,
}

/// NUL-terminated ASCII hits of `text`, as RVAs.
fn find_c_strings(img: &FileImage, text: &str) -> Vec<usize> {
    let raw = text.as_bytes();
    let mut hits = Vec::new();
    let mut start = 0usize;
    while start + raw.len() < img.bytes.len() {
        let Some(pos) = img.bytes[start..].windows(raw.len()).position(|w| w == raw) else {
            break;
        };
        let idx = start + pos;
        if img.bytes[idx + raw.len()] == 0 {
            // file offset -> rva
            for s in &img.sections {
                if idx >= s.raw_ptr && idx < s.raw_ptr + s.raw_size {
                    hits.push(s.rva + (idx - s.raw_ptr));
                    break;
                }
            }
        }
        start = idx + 1;
    }
    hits
}

/// (insn_rva, target_rva) for every RIP-relative LEA in an exec range.
fn rip_leas(img: &FileImage, range: &Range<usize>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let Some(code) = img.get(range.start, range.len()) else {
        return out;
    };
    let mut i = 0usize;
    while i < code.len() {
        let ml = if code[i] == 0x8D && code.get(i + 1).is_some_and(|&b| RIP_LEA_RM.contains(&b)) {
            2
        } else if (0x40..=0x4F).contains(&code[i])
            && code.get(i + 1) == Some(&0x8D)
            && code.get(i + 2).is_some_and(|&b| RIP_LEA_RM.contains(&b))
        {
            3
        } else {
            i += 1;
            continue;
        };
        if i + ml + 4 <= code.len() {
            let disp = i32le(code, i + ml);
            let insn = range.start + i;
            let target = (insn as i64 + ml as i64 + 4 + disp as i64) as usize;
            out.push((insn, target));
            i += ml;
        } else {
            i += 1;
        }
    }
    out
}

fn anchor_xrefs(img: &FileImage, text: &str) -> Vec<usize> {
    let mut want = std::collections::HashSet::new();
    for r in find_c_strings(img, text) {
        want.insert(r);
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for range in img.exec_ranges() {
        for (insn, target) in rip_leas(img, &range) {
            if want.contains(&target) && seen.insert(insn) {
                out.push(insn);
            }
        }
    }
    out
}

/// Bound the function containing `inside_rva` via CC padding runs (>=4).
fn function_span(img: &FileImage, code_rva: usize, inside_rva: usize) -> Option<(usize, usize)> {
    let code = img.get(code_rva, 0x2000000)?;
    let off = inside_rva.checked_sub(code_rva)?;
    if off >= code.len() {
        return None;
    }
    let window = 0x9000usize;
    let back_from = off.saturating_sub(window);
    let back = code.get(back_from..off)?;
    let mut start = None;
    let mut i = 0usize;
    while i < back.len() {
        if back[i] == 0xCC {
            let mut e = i;
            while e < back.len() && back[e] == 0xCC {
                e += 1;
            }
            if e - i >= 4 {
                start = Some(code_rva + back_from + e);
            }
            i = e;
        } else {
            i += 1;
        }
    }
    let start = start?;
    let fwd = code.get(off..(off + window).min(code.len()))?;
    let mut j = 1usize;
    while j < fwd.len() {
        if fwd[j] == 0xCC {
            let mut e = j;
            while e < fwd.len() && fwd[e] == 0xCC {
                e += 1;
            }
            if e - j >= 4 {
                return Some((start, inside_rva + j));
            }
            j = e;
        } else {
            j += 1;
        }
    }
    None
}

/// RVAs of movss loads reading exactly 0.125 inside a blob.
fn scale_loads(img: &FileImage, start: usize, end: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let Some(code) = img.get(start, end - start) else {
        return out;
    };
    let mut i = 0usize;
    while i + 8 <= code.len() {
        let op = code[i + 2];
        let modrm = code[i + 3];
        if code[i] == 0xF3
            && code[i + 1] == 0x0F
            && (op == 0x10 || op == 0x59)
            && modrm & 0xC7 == 0x05
        {
            let disp = i32le(code, i + 4);
            let load = start + i;
            let addr = (load as i64 + 8 + disp as i64) as usize;
            if let Some(b) = img.get(addr, 4) {
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if (v - PAGE_SCALE).abs() < 1e-6 {
                    out.push(load);
                }
            }
            i += 8;
            continue;
        }
        i += 1;
    }
    out
}

/// Redirect the 0.125 loads in a function span to a cave (tail CCs)
// holding 1/items.
fn plan_scale_in(
    img: &FileImage,
    _code_rva: usize,
    span: (usize, usize),
    items: u32,
) -> Option<ScaleRedirect> {
    if items == 0 {
        return None;
    }
    let (start, end) = span;
    if img.get(end, 4)? != [0xCC, 0xCC, 0xCC, 0xCC] {
        return None;
    }
    let loads = scale_loads(img, start, end);
    if loads.is_empty() {
        return None;
    }
    let scale = 1.0f32 / items as f32;
    let mut patches = Vec::new();
    for lr in loads {
        let old = img.get(lr, 8)?.to_vec();
        if old.len() != 8 || old[0..2] != [0xF3, 0x0F] || old[3] & 0xC7 != 0x05 {
            return None;
        }
        let disp = cave_rva_sub(end, lr)?;
        let mut new = old[..4].to_vec();
        new.extend_from_slice(&disp.to_le_bytes());
        if new[..4] != old[..4] || new[4..] == old[4..] {
            return None;
        }
        patches.push((lr, old, new));
    }
    Some(ScaleRedirect {
        cave_rva: end,
        scale,
        patches,
    })
}

fn cave_rva_sub(cave: usize, load: usize) -> Option<i32> {
    let d = cave as i64 - (load as i64 + 8);
    if d < i32::MIN as i64 || d > i32::MAX as i64 {
        return None;
    }
    Some(d as i32)
}

fn call_targets(img: &FileImage, code_rva: usize, start: usize, end: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let Some(code) = img.get(code_rva, 0x2000000) else {
        return out;
    };
    let mut o = start - code_rva;
    let lim = (end - code_rva).saturating_sub(5);
    while o < lim && o + 5 <= code.len() {
        if code[o] == 0xE8 {
            let disp = i32le(code, o + 1);
            let t = code_rva + o + 5 + disp as usize;
            if t > code_rva && t < code_rva + code.len() && code[t - code_rva - 1] == 0xCC {
                out.push(t);
            }
        }
        o += 1;
    }
    out
}

fn find_slot_filler(
    img: &FileImage,
    transfer_anchor: &str,
    slot_format: &str,
    policy: XrefPolicy,
) -> Option<(usize, usize)> {
    let (code_rva, _) = img.text_range()?;
    let tx = anchor_xrefs(img, transfer_anchor);
    if tx.is_empty() {
        return None;
    }
    // Unique keeps the pre-Forsaken fail-closed rule: exactly one xref.
    // AnyOuterWithScale tries every xref outer (Forsaken added a second
    // TRANSFER_RECEIVE_DESC string-table xref that never calls the slot
    // filler) and keeps the single distinct slot-filler span found.
    if policy == XrefPolicy::Unique && tx.len() != 1 {
        return None;
    }
    let slot_refs = anchor_xrefs(img, slot_format);
    if slot_refs.is_empty() {
        return None;
    }
    let mut found: Option<(usize, usize)> = None;
    for xref in tx {
        let outer = match function_span_by_rva(img, code_rva, xref) {
            Some(o) => o,
            None => {
                if policy == XrefPolicy::Unique {
                    return None;
                }
                continue;
            }
        };
        for t in call_targets(img, code_rva, outer.0, outer.1) {
            let span = match function_span_by_rva(img, code_rva, t) {
                Some(s) => s,
                None => {
                    if policy == XrefPolicy::Unique {
                        return None;
                    }
                    continue;
                }
            };
            if span.0 != t {
                continue;
            }
            if slot_refs.iter().any(|&r| span.0 <= r && r < span.1) {
                match found {
                    None => found = Some(span),
                    Some(prev) if prev == span => {}
                    _ => return None,
                }
            }
        }
    }
    found
}

fn function_span_by_rva(img: &FileImage, code_rva: usize, inside: usize) -> Option<(usize, usize)> {
    function_span(img, code_rva, inside)
}

fn plan_scale_redirect(
    img: &FileImage,
    code_rva: usize,
    anchor: &str,
    items: u32,
    policy: XrefPolicy,
) -> Option<ScaleRedirect> {
    if items < 1 {
        return None;
    }
    // Unique keeps the old rule (exactly one xref). AnyOuterWithScale
    // tries every xref outer (Forsaken added a second
    // TRANSFER_RECEIVE_DESC xref outside the dots function) and keeps
    // the single outer that yields a scale plan.
    let refs = anchor_xrefs(img, anchor);
    if refs.is_empty() {
        return None;
    }
    if policy == XrefPolicy::Unique {
        if refs.len() != 1 {
            return None;
        }
        let span = function_span_by_rva(img, code_rva, refs[0])?;
        return plan_scale_in(img, code_rva, span, items);
    }
    let mut found = None;
    for r in refs {
        let span = match function_span_by_rva(img, code_rva, r) {
            Some(s) => s,
            None => continue,
        };
        match plan_scale_in(img, code_rva, span, items) {
            Some(p) => {
                if found.is_some() {
                    return None;
                }
                found = Some(p);
            }
            None => continue,
        }
    }
    found
}

fn plan_slot_filler(
    img: &FileImage,
    code_rva: usize,
    items: u32,
    transfer_anchor: &str,
    slot_format: &str,
    policy: XrefPolicy,
) -> Option<ScaleRedirect> {
    if !(1..=127).contains(&items) {
        return None;
    }
    let span = find_slot_filler(img, transfer_anchor, slot_format, policy)?;
    let mut plan = plan_scale_in(img, code_rva, span, items)?;
    if plan.patches.len() != 1 {
        return None;
    }
    let (start, end) = span;
    let body = img.get(start, end - start)?.to_vec();
    if body
        .windows(NEXT_PAGE.len())
        .filter(|w| *w == NEXT_PAGE)
        .count()
        != 1
        || body
            .windows(PREV_PAGE.len())
            .filter(|w| *w == PREV_PAGE)
            .count()
            != 1
    {
        return None;
    }
    let step = items as u8;
    let ni = body.windows(NEXT_PAGE.len()).position(|w| w == NEXT_PAGE)?;
    let pi = body.windows(PREV_PAGE.len()).position(|w| w == PREV_PAGE)?;
    plan.patches.push((
        start + ni,
        NEXT_PAGE[..6].to_vec(),
        vec![0x6B, 0xC9, step, 0x83, 0xC0, step],
    ));
    plan.patches.push((
        start + pi,
        PREV_PAGE.to_vec(),
        vec![0x83, 0xC1, (0u32.wrapping_sub(items) & 0xFF) as u8],
    ));
    Some(plan)
}

pub struct PagePlan {
    pub label: &'static str,
    pub cave_rva: usize,
    pub scale: f32,
    pub patches: Vec<(usize, Vec<u8>, Vec<u8>)>,
}

/// Plan the slot filler (required) plus the two page-dot screens
/// (best effort). Anchors and the xref policy come from the mod spec,
/// so string renames and duplicate xrefs are fixed in `mod.toml`.
/// Missing dots stay vanilla with a warning while the slot pages
/// still apply.
pub fn plan_all(
    img: &FileImage,
    items: u32,
    transfer_anchor: &str,
    craft_anchor: &str,
    slot_format: &str,
    policy: XrefPolicy,
) -> Result<(Vec<PagePlan>, Vec<String>), String> {
    let (code_rva, _) = img.text_range().ok_or("no .text in image")?;
    let mut plans = Vec::new();
    let mut warnings = Vec::new();
    let sf = plan_slot_filler(img, code_rva, items, transfer_anchor, slot_format, policy).ok_or(
        format!("slot filler (called from {transfer_anchor}, uses {slot_format}) was not found"),
    )?;
    plans.push(PagePlan {
        label: "Slot filler",
        cave_rva: sf.cave_rva,
        scale: sf.scale,
        patches: sf.patches,
    });
    for (anchor, label) in [
        (transfer_anchor, "Transfer Items dots"),
        (craft_anchor, "Craft Product dots"),
    ] {
        match plan_scale_redirect(img, code_rva, anchor, items, policy) {
            Some(p) => plans.push(PagePlan {
                label,
                cave_rva: p.cave_rva,
                scale: p.scale,
                patches: p.patches,
            }),
            None => {
                let refs = anchor_xrefs(img, anchor);
                if refs.len() != 1 {
                    warnings.push(format!(
                        "{label}: anchor {anchor} has {} xrefs, want 1",
                        refs.len()
                    ));
                } else {
                    warnings.push(format!("{label}: no 0.125 scale load in {anchor} function"));
                }
            }
        }
    }
    Ok((plans, warnings))
}
