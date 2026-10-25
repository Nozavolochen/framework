//! The five v1 patch operations, run against spec parameters.
//!
//! Algorithms are ported from the proven per-feature implementations:
//! every patch is runtime-computed (jump targets re-derived from the
//! existing rel32), so no absolute address is baked in. Each returns
//! Ok(detail with the applied RVA) or Err(reason); the caller logs
//! either. Nothing here panics.

use retour::RawDetour;

use crate::log::Log;
use crate::patch;
use crate::spec::Op;

fn text() -> Result<(usize, usize), String> {
    patch::host_text()
}

fn unique(pat: &[u8], mask: &[u8]) -> Result<usize, String> {
    let (base, len) = text()?;
    let hits = patch::scan(base, len, pat, mask);
    match hits.len() {
        1 => Ok(hits[0]),
        0 => Err("pattern matched nothing (game logic changed?)".into()),
        n => Err(format!("{n} ambiguous candidates (refused for safety)")),
    }
}

fn run_write_bytes(
    pat: &[u8],
    mask: &[u8],
    offset: usize,
    expect: Option<&[u8]>,
    bytes: &[u8],
) -> Result<String, String> {
    let hit = unique(pat, mask)?;
    let at = hit + offset;
    let cur = patch::read_bytes(at, bytes.len());
    if let Some(e) = expect {
        if cur.len() < e.len() || &cur[..e.len()] != e {
            return Err(format!(
                "precondition bytes changed (wanted {e:02x?} prefix, found {cur:02x?})"
            ));
        }
    }
    patch::write_bytes(at, bytes, Some(&cur))?;
    Ok(format!("@ rva {} [pattern match]", patch::rva_of(at)))
}

fn run_force_branch(pat: &[u8], mask: &[u8], offset: usize) -> Result<String, String> {
    let hit = unique(pat, mask)?;
    let at = hit + offset;
    let cur = patch::read_bytes(at, 6);
    if cur[0] != 0x0f || cur[1] != 0x84 {
        return Err("skip-branch is not je as expected".into());
    }
    let old_rel = patch::read_i32_le(at + 2) as isize;
    let target = (at + 6).wrapping_add(old_rel as usize);
    let new_rel = target.wrapping_sub(at + 5) as i32;
    let mut buf = [0u8; 6];
    buf[0] = 0xe9;
    buf[1..5].copy_from_slice(&new_rel.to_le_bytes());
    buf[5] = 0x90;
    patch::write_bytes(at, &buf, Some(&cur))?;
    Ok(format!("@ rva {} [pattern match]", patch::rva_of(at)))
}

fn run_retarget_jmp(pat: &[u8], mask: &[u8], offset: usize) -> Result<String, String> {
    let (base, len) = text()?;
    let hits = patch::scan(base, len, pat, mask);
    if hits.is_empty() {
        return Err("pattern matched nothing (game logic changed?)".into());
    }
    let mut cands = Vec::new();
    for h in hits {
        let at = h + offset;
        let cur = patch::read_bytes(at, 5);
        if cur[0] != 0xe9 {
            continue;
        }
        let rel = patch::read_i32_le(at + 1) as isize;
        if rel >= 0 {
            continue; // sibling cases jump forward; we want the backward one
        }
        let target = (at + 5).wrapping_add(rel as usize);
        let tb = patch::read_bytes(target, 8);
        // test al,al (84 c0) followed by je, either near (0f 84 + rel32)
        // or short (74 + rel8). Derive the check's skip destination.
        if tb.len() < 8 || tb[0] != 0x84 || tb[1] != 0xc0 {
            continue;
        }
        let skip = if tb[2] == 0x0f && tb[3] == 0x84 {
            let skip_rel = patch::read_i32_le(target + 4) as isize;
            (target + 8).wrapping_add(skip_rel as usize)
        } else if tb[2] == 0x74 {
            (target + 4).wrapping_add(tb[3] as i8 as isize as usize)
        } else {
            continue;
        };
        cands.push((at, skip));
    }
    match cands.len() {
        1 => {
            let (at, skip) = cands[0];
            let new_rel = skip.wrapping_sub(at + 5) as i32;
            let mut buf = [0u8; 5];
            buf[0] = 0xe9;
            buf[1..5].copy_from_slice(&new_rel.to_le_bytes());
            let cur = patch::read_bytes(at, 5);
            patch::write_bytes(at, &buf, Some(&cur))?;
            Ok(format!("@ rva {} [pattern match]", patch::rva_of(at)))
        }
        0 => Err("no backward-jump case into a test al,al check".into()),
        n => Err(format!("{n} ambiguous candidates (refused for safety)")),
    }
}

fn host_exe_path() -> Result<String, String> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameA;
    unsafe {
        let mut buf = [0u8; MAX_PATH as usize];
        let n = GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), MAX_PATH as u32);
        if n == 0 {
            return Err("host path unknown".into());
        }
        Ok(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }
}

fn run_page_size_override(items: u32) -> Result<String, String> {
    use crate::image;
    if items == 8 {
        return Ok("8 items per page, the game default. Nothing patched.".into());
    }
    let exe = host_exe_path()?;
    let img = image::FileImage::load(&exe)?;
    let plans = image::plan_all(&img, items).map_err(|e| format!("left vanilla: {e}"))?;

    // Verify everything against live memory first (all-or-none).
    unsafe {
        let base = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(std::ptr::null_mut())
            as usize;
        if base == 0 {
            return Err("host base unknown".into());
        }
        for p in &plans {
            for (rva, old, _) in &p.patches {
                let va = base + rva;
                let cur = std::slice::from_raw_parts(va as *const u8, old.len()).to_vec();
                if cur != *old {
                    return Err(format!(
                        "left vanilla: {} code at {:#x} did not match NMS.exe",
                        p.label, rva
                    ));
                }
            }
        }
        // All verified: write caves then patches.
        for p in &plans {
            let cave = base + p.cave_rva;
            patch::write_bytes(cave, &p.scale.to_le_bytes(), Some(&[0xCC; 4]))
                .map_err(|e| format!("cave write failed: {e}"))?;
            for (rva, old, new) in &p.patches {
                let va = base + rva;
                patch::write_bytes(va, new, Some(old)).map_err(|e| format!("{}: {e}", p.label))?;
            }
        }
    }
    Ok(format!(
        "{items} items per page on Transfer Items and Craft Product"
    ))
}

// DoInventorySlots hook: 15 args, so RawDetour (retour caps at 14).
type DoSlotsFn = unsafe extern "system" fn(
    u64,
    u64,
    u64,
    u64,
    i32,
    i32,
    u8,
    u64,
    i32,
    i32,
    u8,
    i32,
    i32,
    u8,
    f32,
) -> u64;

const VANILLA_SLOT: i32 = 90;

// UI element struct offsets (GcNGuiElementData).
const ELEMENT_DATA: usize = 0x48;
const DATA_ID: usize = 0x48;
const LAYOUT_W: usize = 0x34;
const MIN_ADDR: usize = 0x10000;

static mut TRAMPOLINE: *const () = std::ptr::null();
static mut DETOUR: Option<RawDetour> = None;
static mut HOOK_SLOT: i32 = 63;
static mut HOOK_CARGO: i32 = 7;
static mut HOOK_TECH: i32 = 4;
static mut HOOK_PANEL_W: f64 = 724.5;

fn read_ptr(addr: usize) -> Option<usize> {
    if addr < MIN_ADDR {
        return None;
    }
    unsafe {
        let p = *(addr as *const usize);
        if p < MIN_ADDR {
            None
        } else {
            Some(p)
        }
    }
}

fn read_ascii16(addr: usize) -> Option<String> {
    if addr < MIN_ADDR {
        return None;
    }
    unsafe {
        let raw = std::slice::from_raw_parts(addr as *const u8, 16);
        let end = raw.iter().position(|&b| b == 0).unwrap_or(16);
        let s = std::str::from_utf8(&raw[..end]).ok()?;
        if s.is_empty() || !s.chars().all(|c| c.is_ascii() && !c.is_control()) {
            return None;
        }
        Some(s.to_string())
    }
}

fn read_f32(addr: usize) -> Option<f32> {
    if addr < MIN_ADDR {
        return None;
    }
    unsafe {
        let v = *(addr as *const f32);
        if v.is_finite() {
            Some(v)
        } else {
            None
        }
    }
}

/// (rows,) if this call draws our scaled cargo/tech grid, else None.
fn override_rows(slot_width: i32, render_height: i32, layer: u64) -> Option<i32> {
    if slot_width != VANILLA_SLOT {
        return None;
    }
    let (cargo, tech) = unsafe { (HOOK_CARGO, HOOK_TECH) };
    let rows = match render_height {
        5 => cargo,
        3 => tech,
        _ => return None,
    };
    if layer == 0 {
        return None;
    }
    let data = read_ptr(layer as usize + ELEMENT_DATA)?;
    let ident = read_ascii16(data + DATA_ID)?;
    if ident != "INVENTORY" {
        return None;
    }
    let w = read_f32(data + LAYOUT_W)? as f64;
    if (w - unsafe { HOOK_PANEL_W }).abs() > 0.5 {
        return None;
    }
    Some(rows)
}

unsafe extern "system" fn detour(
    page: u64,
    layer: u64,
    inventory: u64,
    empty_slot_actions: u64,
    slots_wide: i32,
    popup_actions: i32,
    disabled: u8,
    min_index: u64,
    slot_width: i32,
    slot_height: i32,
    can_hide_slots: u8,
    render_width: i32,
    render_height: i32,
    compare_special: u8,
    scroll_offset: f32,
) -> u64 {
    if TRAMPOLINE.is_null() {
        return 0;
    }
    let orig: DoSlotsFn = std::mem::transmute(TRAMPOLINE);
    let slot = HOOK_SLOT;
    match override_rows(slot_width, render_height, layer) {
        Some(rows) => orig(
            page,
            layer,
            inventory,
            empty_slot_actions,
            slots_wide,
            popup_actions,
            disabled,
            min_index,
            slot,
            slot,
            can_hide_slots,
            render_width,
            rows,
            compare_special,
            scroll_offset,
        ),
        None => orig(
            page,
            layer,
            inventory,
            empty_slot_actions,
            slots_wide,
            popup_actions,
            disabled,
            min_index,
            slot_width,
            slot_height,
            can_hide_slots,
            render_width,
            render_height,
            compare_special,
            scroll_offset,
        ),
    }
}

fn run_detour(
    pat: &[u8],
    mask: &[u8],
    slot_width: i32,
    cargo_rows: i32,
    tech_rows: i32,
    panel_width: f64,
) -> Result<String, String> {
    let (base, len) = text()?;
    let hits = patch::scan(base, len, pat, mask);
    if hits.len() != 1 {
        if hits.is_empty() {
            return Err("slot renderer hook pattern matched nothing (game update?)".into());
        }
        return Err(format!(
            "{} ambiguous hook candidates (refused for safety)",
            hits.len()
        ));
    }
    let target = hits[0];
    unsafe {
        HOOK_SLOT = slot_width;
        HOOK_CARGO = cargo_rows;
        HOOK_TECH = tech_rows;
        HOOK_PANEL_W = panel_width;
        let d = RawDetour::new(target as *const (), detour as *const ())
            .map_err(|e| format!("detour create failed: {e:?}"))?;
        d.enable()
            .map_err(|e| format!("detour enable failed: {e:?}"))?;
        TRAMPOLINE = d.trampoline();
        DETOUR = Some(d);
    }
    Ok(format!(
        "slot renderer hooked @ rva {} [pattern match]",
        patch::rva_of(target)
    ))
}

/// Run one spec op. `bytes_name` is used only for NOP6 shorthand.
pub fn run(op: &Op, _log: &mut Log) -> Result<String, String> {
    match op {
        Op::WriteBytes {
            pat,
            mask,
            offset,
            expect,
            bytes,
        } => run_write_bytes(pat, mask, *offset, expect.as_deref(), bytes),
        Op::ForceBranch { pat, mask, offset } => run_force_branch(pat, mask, *offset),
        Op::RetargetJmp { pat, mask, offset } => run_retarget_jmp(pat, mask, *offset),
        Op::PageSizeOverride { items } => run_page_size_override(*items),
        Op::DetourWithArgOverrides {
            pat,
            mask,
            slot_width,
            cargo_rows,
            tech_rows,
            panel_width,
        } => run_detour(
            pat,
            mask,
            *slot_width,
            *cargo_rows,
            *tech_rows,
            *panel_width,
        ),
    }
}
