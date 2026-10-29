//! The patch operations, run against spec parameters.
//!
//! Algorithms are ported from the proven per-feature implementations:
//! every patch is runtime-computed (jump targets re-derived from the
//! existing rel32), so no absolute address is baked in. Each returns
//! Ok(detail with the applied RVA) or Err(reason); the caller logs
//! either. Nothing here panics.

use retour::RawDetour;

use crate::log::Log;
use crate::patch;
use crate::spec::{Op, Patch, Setting, XrefPolicy};

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
    id: &str,
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
    applied_record(id, at, cur);
    Ok(format!("@ rva {} [pattern match]", patch::rva_of(at)))
}

fn run_force_branch(id: &str, pat: &[u8], mask: &[u8], offset: usize) -> Result<String, String> {
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
    applied_record(id, at, cur);
    Ok(format!("@ rva {} [pattern match]", patch::rva_of(at)))
}

fn run_retarget_jmp(id: &str, pat: &[u8], mask: &[u8], offset: usize) -> Result<String, String> {
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
            applied_record(id, at, cur);
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

/// Applied page plans, for restore-then-reapply on retune:
/// (cave originals, patch originals).
static PAGES: OnceLock<Mutex<Option<AppliedPages>>> = OnceLock::new();

struct AppliedPages {
    caves: Vec<(usize, Vec<u8>)>,
    patches: Vec<(usize, Vec<u8>)>,
}

fn pages_lock() -> &'static Mutex<Option<AppliedPages>> {
    PAGES.get_or_init(|| Mutex::new(None))
}

/// Restore whatever a previous page plan wrote, if anything.
fn pages_restore() -> Result<bool, String> {
    let prev = pages_lock()
        .lock()
        .map_err(|_| "page cache lock poisoned".to_string())?
        .take();
    let Some(applied) = prev else {
        return Ok(false);
    };
    for (addr, bytes) in applied.caves.iter().chain(applied.patches.iter()) {
        patch::write_bytes(*addr, bytes, None)?;
    }
    Ok(true)
}

fn pages_apply(
    items: u32,
    transfer_anchor: &str,
    craft_anchor: &str,
    slot_format: &str,
    policy: XrefPolicy,
) -> Result<String, String> {
    use crate::image;
    let exe = host_exe_path()?;
    let img = image::FileImage::load(&exe)?;
    let (plans, warnings) = image::plan_all(
        &img,
        items,
        transfer_anchor,
        craft_anchor,
        slot_format,
        policy,
    )
    .map_err(|e| format!("left vanilla: {e}"))?;

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
        // All verified: snapshot cave originals, write caves + patches.
        let mut caves = Vec::new();
        let mut olds = Vec::new();
        for p in &plans {
            let cave = base + p.cave_rva;
            caves.push((cave, patch::read_bytes(cave, 4)));
            patch::write_bytes(cave, &p.scale.to_le_bytes(), Some(&[0xCC; 4]))
                .map_err(|e| format!("cave write failed: {e}"))?;
            for (rva, old, new) in &p.patches {
                let va = base + rva;
                olds.push((va, old.clone()));
                patch::write_bytes(va, new, Some(old)).map_err(|e| format!("{}: {e}", p.label))?;
            }
        }
        if let Ok(mut guard) = pages_lock().lock() {
            *guard = Some(AppliedPages {
                caves,
                patches: olds,
            });
        }
    }
    Ok(if warnings.is_empty() {
        format!("{items} items per page on Transfer Items and Craft Product")
    } else {
        format!(
            "{items} items per page on Transfer Items and Craft Product (dots left vanilla: {})",
            warnings.join("; ")
        )
    })
}

/// Retune the page size: restore the previous plan, then apply fresh
/// (or stay vanilla when off or at the 8-item default).
fn retune_pages(
    _id: &str,
    on: bool,
    items: i64,
    transfer_anchor: &str,
    craft_anchor: &str,
    slot_format: &str,
    policy: XrefPolicy,
) -> Result<String, String> {
    if items < 1 || items > 127 {
        return Err(format!("items {items} outside 1 to 127"));
    }
    let restored = pages_restore()?;
    if !on || items == 8 {
        return Ok(if restored {
            "restored vanilla (8 per page)".into()
        } else {
            "8 items per page, the game default. Nothing patched.".into()
        });
    }
    pages_apply(
        items as u32,
        transfer_anchor,
        craft_anchor,
        slot_format,
        policy,
    )
}

fn run_page_size_override(
    items: u32,
    transfer_anchor: &str,
    craft_anchor: &str,
    slot_format: &str,
    policy: XrefPolicy,
) -> Result<String, String> {
    if items == 8 {
        return Ok("8 items per page, the game default. Nothing patched.".into());
    }
    pages_apply(items, transfer_anchor, craft_anchor, slot_format, policy)
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
        Some(rows) => {
            // The game computes the scroll offset in vanilla-slot pixels
            // (90px). With smaller slots the same offset scrolls too far
            // (one vanilla row = 90px lands mid-row at 63px), clipping the
            // top row. Scale into modded-slot pixels so vanilla-aligned
            // positions (90*k) stay aligned (63*k).
            let scaled_scroll = scroll_offset * (slot as f32 / VANILLA_SLOT as f32);
            orig(
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
                scaled_scroll,
            )
        }
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

struct DetourTuning<'a> {
    pat: &'a [u8],
    mask: &'a [u8],
    slot_width: i32,
    cargo_rows: i32,
    tech_rows: i32,
    panel_width: f64,
}

fn detour_store(t: &DetourTuning) {
    unsafe {
        HOOK_SLOT = t.slot_width;
        HOOK_CARGO = t.cargo_rows;
        HOOK_TECH = t.tech_rows;
        HOOK_PANEL_W = t.panel_width;
    }
}

fn detour_detail(t: &DetourTuning, at: usize) -> String {
    format!(
        "slot renderer hooked @ rva {} [pattern match] (slot {}, cargo {}, tech {}, panel {:.1})",
        patch::rva_of(at),
        t.slot_width,
        t.cargo_rows,
        t.tech_rows,
        t.panel_width
    )
}

/// Whether the slot detour is installed (TRAMPOLINE tracks DETOUR:
/// both are set together on install and never cleared).
fn detour_installed() -> bool {
    unsafe { !TRAMPOLINE.is_null() }
}

fn detour_enable() -> Result<(), String> {
    unsafe {
        let slot: *const Option<RawDetour> = std::ptr::addr_of!(DETOUR);
        match &*slot {
            Some(d) => d
                .enable()
                .map_err(|e| format!("detour enable failed: {e:?}")),
            None => Err("detour not installed".into()),
        }
    }
}

fn detour_disable() -> Result<(), String> {
    unsafe {
        let slot: *const Option<RawDetour> = std::ptr::addr_of!(DETOUR);
        match &*slot {
            Some(d) => d
                .disable()
                .map_err(|e| format!("detour disable failed: {e:?}")),
            None => Err("detour not installed".into()),
        }
    }
}

/// Retune the inventory detour: the hook reads its tuning from the
/// HOOK_* globals every call, so new values apply instantly. Toggling
/// off disables the detour; toggling back on re-enables it (or
/// installs it fresh when boot left it off).
fn retune_detour(_id: &str, on: bool, t: DetourTuning) -> Result<String, String> {
    detour_store(&t);
    if !detour_installed() {
        if !on {
            return Ok("already vanilla".into());
        }
        let at = install_detour(&t)?;
        return Ok(detour_detail(&t, at));
    }
    if on {
        detour_enable()?;
        let at = hook_addr(t.pat, t.mask)?;
        Ok(detour_detail(&t, at))
    } else {
        detour_disable()?;
        Ok("detour disabled, vanilla renderer".into())
    }
}

fn hook_addr(pat: &[u8], mask: &[u8]) -> Result<usize, String> {
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
    Ok(hits[0])
}

fn install_detour(t: &DetourTuning) -> Result<usize, String> {
    let target = hook_addr(t.pat, t.mask)?;
    unsafe {
        let d = RawDetour::new(target as *const (), detour as *const ())
            .map_err(|e| format!("detour create failed: {e:?}"))?;
        d.enable()
            .map_err(|e| format!("detour enable failed: {e:?}"))?;
        TRAMPOLINE = d.trampoline();
        DETOUR = Some(d);
    }
    Ok(target)
}

/// Live-reload entry: push changed tuning into an already applied
/// patch. Pattern changes never travel this path (hooks do not move);
/// only tuning keys are re-applied.
pub fn retune(patch: &Patch, settings: &[Setting], enabled: bool) -> Result<String, String> {
    let on = enabled && bool_setting(settings, &patch.id, true);
    match &patch.op {
        Op::WriteBytes {
            pat,
            mask,
            offset,
            expect,
            bytes,
        } => retune_bytes(&patch.id, on, &mut || {
            run_write_bytes(&patch.id, pat, mask, *offset, expect.as_deref(), bytes)
        }),
        Op::ForceBranch { pat, mask, offset } => retune_bytes(&patch.id, on, &mut || {
            run_force_branch(&patch.id, pat, mask, *offset)
        }),
        Op::RetargetJmp { pat, mask, offset } => retune_bytes(&patch.id, on, &mut || {
            run_retarget_jmp(&patch.id, pat, mask, *offset)
        }),
        Op::PageSizeOverride {
            items,
            transfer_anchor,
            craft_anchor,
            slot_format,
            xref_policy,
        } => retune_pages(
            &patch.id,
            on,
            int_setting(settings, "items").unwrap_or(*items as i64),
            transfer_anchor,
            craft_anchor,
            slot_format,
            *xref_policy,
        ),
        Op::DetourWithArgOverrides {
            pat,
            mask,
            slot_width,
            cargo_rows,
            tech_rows,
            panel_width,
        } => retune_detour(
            &patch.id,
            on,
            DetourTuning {
                pat,
                mask,
                slot_width: int_setting(settings, "slot_width").unwrap_or(*slot_width as i64)
                    as i32,
                cargo_rows: int_setting(settings, "cargo_rows").unwrap_or(*cargo_rows as i64)
                    as i32,
                tech_rows: int_setting(settings, "tech_rows").unwrap_or(*tech_rows as i64) as i32,
                panel_width: float_setting(settings, "panel_width").unwrap_or(*panel_width),
            },
        ),
    }
}

/// Read one setting's live value, parsed. Unknown ids and unparsable
/// values yield None; callers fall back to the patch key default.
fn find_setting<'a>(settings: &'a [Setting], id: &str) -> Option<&'a Setting> {
    settings.iter().find(|s| s.id == id)
}

fn bool_setting(settings: &[Setting], id: &str, default: bool) -> bool {
    match find_setting(settings, id) {
        None => default,
        Some(s) => match s.current().trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => default,
        },
    }
}

fn int_setting(settings: &[Setting], id: &str) -> Option<i64> {
    find_setting(settings, id)?.current().trim().parse().ok()
}

fn float_setting(settings: &[Setting], id: &str) -> Option<f64> {
    find_setting(settings, id)?.current().trim().parse().ok()
}

/// Whether a patch runs: mod on, and no bool setting with the
/// patch's id switching it off. Shared by boot and reload.
pub fn patch_on(spec: &crate::spec::Mod, patch: &Patch) -> bool {
    spec.enabled && bool_setting(&spec.settings, &patch.id, true)
}

// ---------------------------------------------------------------------------
// Byte patches: apply/restore with original-byte cache.
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Patch id -> (address, original bytes) for every applied byte patch.
/// Disabling a patch writes the originals back; the entry is removed.
static APPLIED: OnceLock<Mutex<HashMap<String, Vec<(usize, Vec<u8>)>>>> = OnceLock::new();

fn applied_lock() -> &'static Mutex<HashMap<String, Vec<(usize, Vec<u8>)>>> {
    APPLIED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn applied_record(id: &str, addr: usize, original: Vec<u8>) {
    if let Ok(mut guard) = applied_lock().lock() {
        guard.insert(id.to_string(), vec![(addr, original)]);
    }
}

fn applied_take(id: &str) -> Option<Vec<(usize, Vec<u8>)>> {
    applied_lock().lock().ok()?.remove(id)
}

fn applied_has(id: &str) -> bool {
    applied_lock()
        .lock()
        .is_ok_and(|guard| guard.contains_key(id))
}

/// Enable/disable a byte patch: run fresh when switching on, write
/// the cached originals back when switching off.
fn retune_bytes(
    id: &str,
    on: bool,
    run: &mut dyn FnMut() -> Result<String, String>,
) -> Result<String, String> {
    if on {
        if applied_has(id) {
            return Ok("already applied".into());
        }
        return run();
    }
    match applied_take(id) {
        None => Ok("already vanilla".into()),
        Some(originals) => {
            for (addr, bytes) in &originals {
                patch::write_bytes(*addr, bytes, None)?;
            }
            Ok("restored vanilla".into())
        }
    }
}
/// Run one spec op. Settings with a tuning key's id override the
/// patch key defaults; a bool setting with the patch's id gates the
/// patch on/off (checked by the caller via patch_on).
pub fn run(patch: &Patch, settings: &[Setting], _log: &mut Log) -> Result<String, String> {
    let op = &patch.op;
    match op {
        Op::WriteBytes {
            pat,
            mask,
            offset,
            expect,
            bytes,
        } => run_write_bytes(&patch.id, pat, mask, *offset, expect.as_deref(), bytes),
        Op::ForceBranch { pat, mask, offset } => run_force_branch(&patch.id, pat, mask, *offset),
        Op::RetargetJmp { pat, mask, offset } => run_retarget_jmp(&patch.id, pat, mask, *offset),
        Op::PageSizeOverride {
            items,
            transfer_anchor,
            craft_anchor,
            slot_format,
            xref_policy,
        } => {
            let n = int_setting(settings, "items").unwrap_or(*items as i64);
            if n < 1 || n > 127 {
                return Err(format!("items {n} outside 1 to 127"));
            }
            run_page_size_override(
                n as u32,
                transfer_anchor,
                craft_anchor,
                slot_format,
                *xref_policy,
            )
        }
        Op::DetourWithArgOverrides {
            pat,
            mask,
            slot_width,
            cargo_rows,
            tech_rows,
            panel_width,
        } => {
            let t = DetourTuning {
                pat,
                mask,
                slot_width: int_setting(settings, "slot_width").unwrap_or(*slot_width as i64)
                    as i32,
                cargo_rows: int_setting(settings, "cargo_rows").unwrap_or(*cargo_rows as i64)
                    as i32,
                tech_rows: int_setting(settings, "tech_rows").unwrap_or(*tech_rows as i64) as i32,
                panel_width: float_setting(settings, "panel_width").unwrap_or(*panel_width),
            };
            detour_store(&t);
            // Boot runs once; a previous install in this process just
            // gets fresh tuning.
            if detour_installed() {
                let at = hook_addr(t.pat, t.mask)?;
                return Ok(detour_detail(&t, at));
            }
            let at = install_detour(&t)?;
            Ok(detour_detail(&t, at))
        }
    }
}
