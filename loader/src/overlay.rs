//! In-game settings overlay.
//!
//! A plain topmost tool window pinned over the game, painted with
//! GDI and driven by keyboard polling on a dedicated input thread
//! (~120 Hz, so quick taps are never missed). It lists every loaded
//! mod's `[[setting]]` entries with bool/enum/float/int rows and
//! basic text editing, grouped under collapsible mod headers.
//!
//! Deliberately NOT a layered window: layered translucency does not
//! composite over the game under Proton, while a normal opaque
//! window raised topmost does. The panel stays click-through
//! (WS_EX_TRANSPARENT) since all input is polled, never focused.
//!
//! Edits rewrite the `value` line in the mod's own `mod.toml`; the
//! existing file-watch reload (every two seconds) applies them live,
//! so the overlay and hand-edits share one write path and one
//! reload path. Tick sources for hook health: the present hook plus
//! a message-pump fallback (the game resolves present through
//! vkGetDeviceProcAddr, bypassing the present IAT slot).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::log::Log;
use crate::spec::{Mod, Setting, SettingKind};

use nozavolochen_strings::Strings;

/// Grave key: the overlay key by physical position (the key left
/// of 1; backtick on US layouts). Virtual-key 0xC0 is
/// layout-dependent (on some layouts the same VK yields another
/// character, and the grave character lives on another VK), so the
/// toggle listens for the scan code and polls whatever VK the active
/// layout maps it to.
const VK_OEM_3: i32 = 0xC0;
/// Scan code set 1 for the grave key (left of 1, DIK_GRAVE).
/// Layout-independent: the same physical key on every layout.
const SC_GRAVE: u32 = 0x29;
/// Tab: switch between the Settings and Mods pages.
const VK_TAB: i32 = 0x09;
const VK_SHIFT: i32 = 0x10;

/// UTF-8 overlay text as NUL-terminated UTF-16 for the W drawing
/// calls. Rust strings are always valid UTF-8, so this cannot fail.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Re-check mod.toml mtimes every two seconds while presenting.
const RELOAD_MS: u64 = 2000;
/// Input thread poll interval: fast enough to catch quick taps.
const INPUT_MS: u32 = 8;
/// Key-repeat: initial delay then rate, in input polls.
const REPEAT_DELAY: u32 = 50;
const REPEAT_RATE: u32 = 10;

static OPEN: AtomicBool = AtomicBool::new(false);
/// Whether the overlay is text-editing (the key hook reads this to
/// swallow printable keys). Set once per frame; the hook only reads.
static EDITING: AtomicBool = AtomicBool::new(false);
/// Key state seen by the hook. A swallowed key never reaches
/// GetAsyncKeyState under Proton, so without this the overlay would
/// go blind the moment capture starts. Polled state is the OR of
/// physical and hook-seen state.
static HOOK_DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
/// Hook-seen state for the grave-key scan code. The polled VK varies
/// with the layout, so the physical key gets its own flag outside the
/// VK-indexed table above.
static SC_GRAVE_DOWN: AtomicBool = AtomicBool::new(false);
/// Whether the Steam overlay is up. Written by the Steam callback
/// (game thread) and read by the input thread. While set, the panel
/// stays closed and swallows nothing: muted like the game.
static STEAM_OPEN: AtomicBool = AtomicBool::new(false);
/// Steam watch state. The game pumps SteamAPI_RunCallbacks, so a
/// GameOverlayActivated_t registration (id 331) is dispatched
/// without any polling. Registration runs once on the input thread.
/// If steam_api64.dll or RegisterCallback is missing (GOG, future
/// SDK), the watch stays off after one log line and the panel
/// behaves as if there were no watch.
static mut STEAM_WATCH_INIT: bool = false;
/// GameOverlayActivated_t: k_iSteamFriendsCallbacks (300) + 31.
const OVERLAY_CALLBACK_ID: i32 = 331;
/// The game client window the panel pins to. The toggle and capture
/// only apply when this exact window is foreground, so typing in a
/// Steam overlay chat (or any other window) never toggles us. Set on
/// first sight; every later sight of another same-process window is
/// treated as not-the-game rather than re-pinning.
static GAME_HWND: AtomicIsize = AtomicIsize::new(0);
static mut PRESENT_ORIG: usize = 0;
static mut PUMP_ORIG: usize = 0;
static mut WAS_DOWN: bool = false;
static mut HOOK: isize = 0;
static mut LAST_POLL_MS: u64 = 0;

static UI: OnceLock<Mutex<Overlay>> = OnceLock::new();
static THREAD_STARTED: OnceLock<()> = OnceLock::new();

fn ui() -> &'static Mutex<Overlay> {
    UI.get_or_init(|| Mutex::new(Overlay::default()))
}

/// Read by the overlay toggle; until the first present call the
/// hooks are not yet installed.
#[allow(dead_code)]
pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// Layout-independent toggle predicate: the physical grave key.
/// Pure so it stays unit-testable without Win32.
fn is_toggle_scancode(sc: u32) -> bool {
    sc == SC_GRAVE
}

fn key_down(vk: i32) -> bool {
    let hooked = (vk as usize) < 256 && HOOK_DOWN[vk as usize].load(Ordering::Relaxed);
    hooked
        || unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) as u16 & 0x8000
                != 0
        }
}

/// Whether the physical grave key is down: hook-seen scan-code state
/// first (swallowed keys are invisible to GetAsyncKeyState under
/// Proton), then whatever VK the active layout maps the scan code
/// to, then the legacy US VK as fallback.
fn toggle_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyA, MAPVK_VSC_TO_VK_EX};
    if SC_GRAVE_DOWN.load(Ordering::Relaxed) {
        return true;
    }
    let vk = unsafe { MapVirtualKeyA(SC_GRAVE, MAPVK_VSC_TO_VK_EX) };
    if vk != 0 && key_down(vk as i32) {
        return true;
    }
    key_down(VK_OEM_3)
}

/// Pure toggle gate for tests: Steam overlay up or game client not
/// foreground both suppress the toggle.
fn toggle_allowed(steam_open: bool, game_foreground: bool) -> bool {
    !steam_open && game_foreground
}

/// Minimal CCallbackBase layout (Steamworks): vtable of Run,
/// Run-with-call-result, and size, then flags and the callback id.
/// Only Run matters here; the payload is one byte (m_bActive).
#[repr(C)]
struct SteamCallbackVTable {
    run: unsafe extern "system" fn(*mut c_void, *mut c_void),
    run_extra: unsafe extern "system" fn(*mut c_void, *mut c_void, bool, u64),
    size: unsafe extern "system" fn(*mut c_void) -> i32,
}

#[repr(C)]
struct SteamCallback {
    vtable: *const SteamCallbackVTable,
    flags: u32,
    callback: i32,
}

/// GameOverlayActivated_t handler: the payload is a single
/// m_bActive byte. Runs on whatever game thread pumps callbacks;
/// only flips the atomic, never logs (input thread reports).
unsafe extern "system" fn steam_overlay_run(_this: *mut c_void, param: *mut c_void) {
    if !param.is_null() {
        STEAM_OPEN.store(*(param as *const u8) != 0, Ordering::Relaxed);
    }
}

unsafe extern "system" fn steam_overlay_run_extra(
    this: *mut c_void,
    param: *mut c_void,
    _failure: bool,
    _call: u64,
) {
    steam_overlay_run(this, param);
}

unsafe extern "system" fn steam_overlay_size(_this: *mut c_void) -> i32 {
    1
}

static STEAM_VTABLE: SteamCallbackVTable = SteamCallbackVTable {
    run: steam_overlay_run,
    run_extra: steam_overlay_run_extra,
    size: steam_overlay_size,
};

static mut STEAM_CALLBACK: SteamCallback = SteamCallback {
    vtable: &STEAM_VTABLE as *const SteamCallbackVTable,
    flags: 0,
    callback: OVERLAY_CALLBACK_ID,
};

/// One-time Steam watch registration on the input thread.
/// Fail-quiet after one log line: without the game pumping callbacks
/// the flag simply never flips.
fn steam_watch_poll() {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    unsafe {
        if !STEAM_WATCH_INIT {
            STEAM_WATCH_INIT = true;
            let api = GetModuleHandleA(b"steam_api64.dll\0".as_ptr());
            if api.is_null() {
                crate::log::append(
                    "nozavolochen.log",
                    "overlay: steam watch off (no steam_api64)",
                );
            } else {
                let reg = GetProcAddress(api, b"SteamAPI_RegisterCallback\0".as_ptr());
                let Some(reg) = reg else {
                    crate::log::append(
                        "nozavolochen.log",
                        "overlay: steam watch off (no RegisterCallback)",
                    );
                    return;
                };
                let reg: unsafe extern "system" fn(*mut c_void, i32) =
                    std::mem::transmute(reg as usize);
                reg(
                    std::ptr::addr_of_mut!(STEAM_CALLBACK) as *mut c_void,
                    OVERLAY_CALLBACK_ID,
                );
                crate::log::append(
                    "nozavolochen.log",
                    "overlay: steam watch on (GameOverlayActivated)",
                );
            }
        }
    }
}

/// Whether the Steam overlay is currently up (throttled poll above).
fn steam_overlay_open() -> bool {
    STEAM_OPEN.load(Ordering::Relaxed)
}

/// Forget hook-seen keys (on close), so a missed key-up can never
/// stick a key down.
fn clear_hook_keys() {
    for slot in HOOK_DOWN.iter() {
        slot.store(false, Ordering::Relaxed);
    }
    SC_GRAVE_DOWN.store(false, Ordering::Relaxed);
}

/// The foreground window when it is this process. Typing in another
/// app while the game runs must not toggle us.
fn same_process_window() -> Option<isize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == windows_sys::Win32::System::Threading::GetCurrentProcessId() {
            Some(hwnd as isize)
        } else {
            None
        }
    }
}

/// The foreground window when it is the game client itself: the exact
/// pinned window, not just any same-process window. A Steam overlay
/// chat is a different window (often a different process), so typing
/// the grave character there no longer toggles the panel. Until the
/// client window has been seen once, the same-process window
/// bootstraps the pin.
fn foreground_window() -> Option<isize> {
    let fg = same_process_window()?;
    let known = GAME_HWND.load(Ordering::Relaxed);
    if known == 0 {
        GAME_HWND.store(fg, Ordering::Relaxed);
        Some(fg)
    } else if fg == known {
        Some(fg)
    } else {
        None
    }
}

/// One adjustable row: a mod's setting plus where to write it back.
#[derive(Debug, Clone)]
struct Row {
    folder: String,
    mod_id: String,
    setting: Setting,
}

impl Row {
    fn current(&self) -> String {
        self.setting
            .value
            .clone()
            .unwrap_or_else(|| self.setting.default.clone())
    }

    fn display(&self, s: &Strings) -> String {
        let cur = self.current();
        match self.setting.kind {
            SettingKind::Bool => {
                if cur.trim().eq_ignore_ascii_case("true") || cur.trim() == "1" {
                    s.get("chrome.on").to_string()
                } else {
                    s.get("chrome.off").to_string()
                }
            }
            SettingKind::Float => match cur.parse::<f64>() {
                Ok(v) => format!("{v:.2}"),
                Err(_) => cur,
            },
            _ => cur,
        }
    }
}

/// One collapsible mod group.
#[derive(Debug, Clone)]
struct Group {
    id: String,
    label: String,
    description: String,
    /// The mod's `[mod] enabled` flag: settings rows of a disabled
    /// mod render dimmed and ignore edits.
    enabled: bool,
    collapsed: bool,
}

/// Overlay pages. Settings is today's adjustable-rows list; Mods
/// lists every installed mod with its on/off switch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Settings,
    Mods,
}

/// One Mods-page row: every loaded mod (including settings-less
/// ones the Settings page skips), plus unloadable folders carrying
/// their parse error.
#[derive(Debug, Clone)]
struct ModEntry {
    folder: String,
    mod_id: String,
    label: String,
    version: String,
    description: String,
    enabled: bool,
    /// Some when the folder failed to parse: shown instead of a switch.
    error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Item {
    Header(usize),
    Row(usize),
}

#[derive(Default)]
struct Overlay {
    hwnd: isize,
    class_registered: bool,
    /// Game window the panel is pinned to.
    game_hwnd: isize,
    /// Latched when window creation fails: stop retrying (and
    /// logging) until the next open.
    window_dead: bool,
    /// Repaint needed (nav, edit, reload, resize).
    dirty: bool,
    page: Page,
    groups: Vec<Group>,
    /// Flat rows and the selected index.
    rows: Vec<Row>,
    /// Selectable items (headers + visible rows).
    items: Vec<Item>,
    selected: usize,
    mod_entries: Vec<ModEntry>,
    mod_selected: usize,
    /// Key state for edge detection + repeat.
    prev: Vec<(i32, bool, u32)>,
    /// Text being typed (setting index, buffer).
    editing: Option<(usize, String)>,
    /// Optimistic values written this session, until the reload
    /// picks them up: ((mod, setting), value, tick).
    pending: Vec<((String, String), String, u64)>,
    /// Optimistic mod on/off switches, until the reload confirms
    /// them: (mod id, enabled, tick).
    pending_mods: Vec<(String, bool, u64)>,
    tick: u64,
    /// Overlay chrome strings in the game's language (rebuilt on open).
    strings: Strings,
}

unsafe impl Send for Overlay {}

/// Edge + repeat state for one key. Returns (pressed, repeated).
fn key_edge(state: &mut Overlay, vk: i32) -> (bool, bool) {
    let down = key_down(vk);
    let slot = state.prev.iter_mut().find(|(k, _, _)| *k == vk);
    let entry = match slot {
        Some(e) => e,
        None => {
            state.prev.push((vk, false, 0));
            state.prev.last_mut().unwrap()
        }
    };
    let held = if down { entry.2 + 1 } else { 0 };
    let was = entry.1;
    entry.1 = down;
    entry.2 = held;
    let edge = down && !was;
    let repeat = down && held > REPEAT_DELAY && (held - REPEAT_DELAY) % REPEAT_RATE == 0;
    (edge, repeat)
}

fn shift_down() -> bool {
    key_down(VK_SHIFT)
}

impl Overlay {
    /// Rebuild groups and rows from the loaded mods, preserving
    /// collapsed groups by mod id. Also rebuilds the Mods-page
    /// entries, which unlike the Settings page include settings-less
    /// mods and unloadable folders.
    fn rebuild(&mut self, mods: &[Mod]) {
        let collapsed: Vec<String> = self
            .groups
            .iter()
            .filter(|g| g.collapsed)
            .map(|g| g.id.clone())
            .collect();
        let mut groups = Vec::new();
        let mut rows = Vec::new();
        for m in mods {
            // The Settings page is a settings UI: mods with nothing
            // to tune stay out (their patches still apply at boot).
            // The Mods page below lists them.
            if !m.settings.is_empty() {
                groups.push(Group {
                    id: m.id.clone(),
                    label: if m.label.is_empty() {
                        m.id.clone()
                    } else {
                        m.label.clone()
                    },
                    description: m.description.clone(),
                    enabled: m.enabled,
                    collapsed: collapsed.contains(&m.id),
                });
                for s in &m.settings {
                    rows.push(Row {
                        folder: String::new(),
                        mod_id: m.id.clone(),
                        setting: s.clone(),
                    });
                }
            }
        }
        // Fill folders from the host's folder-by-id map.
        let map = crate::host::folder_map();
        for r in rows.iter_mut() {
            if let Some(f) = map.iter().find(|(id, _)| *id == r.mod_id) {
                r.folder = f.1.clone();
            }
        }
        rows.retain(|r| !r.folder.is_empty());
        // Drop headers left row-less by a failed folder lookup.
        groups.retain(|g| rows.iter().any(|r| r.mod_id == g.id));
        self.groups = groups;
        self.rows = rows;
        self.rebuild_items();
        self.rebuild_mods(mods, &map);
        self.prune_pending_mods();
        self.dirty = true;
    }

    /// Rebuild the Mods-page entries: every loaded mod, then any
    /// unloadable folders with their parse errors.
    fn rebuild_mods(&mut self, mods: &[Mod], map: &[(String, String)]) {
        let mut entries = Vec::new();
        for m in mods {
            let folder = map
                .iter()
                .find(|(id, _)| *id == m.id)
                .map(|(_, f)| f.clone())
                .unwrap_or_default();
            entries.push(ModEntry {
                folder,
                mod_id: m.id.clone(),
                label: if m.label.is_empty() {
                    m.id.clone()
                } else {
                    m.label.clone()
                },
                version: m.version.clone(),
                description: m.description.clone(),
                enabled: m.enabled,
                error: None,
            });
        }
        // Drop entries with no folder (same failed-lookup rule as rows).
        entries.retain(|e| !e.folder.is_empty());
        for (folder, error) in crate::host::load_errors() {
            if entries.iter().any(|e| e.folder == folder) {
                continue;
            }
            entries.push(ModEntry {
                folder: folder.clone(),
                mod_id: folder.clone(),
                label: folder,
                version: String::new(),
                description: String::new(),
                enabled: false,
                error: Some(error),
            });
        }
        entries.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
        self.mod_entries = entries;
        self.mod_selected = self
            .mod_selected
            .min(self.mod_entries.len().saturating_sub(1));
    }

    fn rebuild_items(&mut self) {
        let mut items = Vec::new();
        for (g, group) in self.groups.iter().enumerate() {
            items.push(Item::Header(g));
            if !group.collapsed {
                for (r, row) in self.rows.iter().enumerate() {
                    if row.mod_id == group.id {
                        items.push(Item::Row(r));
                    }
                }
            }
        }
        self.items = items;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
    }

    fn toggle_group(&mut self, g: usize) {
        if let Some(group) = self.groups.get_mut(g) {
            group.collapsed = !group.collapsed;
        }
        self.rebuild_items();
        self.dirty = true;
    }

    /// Height of one selectable item: headers and rows with a
    /// description take a second line for the byline.
    fn item_height(&self, item: &Item) -> i32 {
        match item {
            Item::Header(g) => {
                if self
                    .groups
                    .get(*g)
                    .is_some_and(|gr| !gr.description.is_empty())
                {
                    44
                } else {
                    26
                }
            }
            Item::Row(r) => {
                if self
                    .rows
                    .get(*r)
                    .is_some_and(|row| !row.setting.description.is_empty())
                {
                    44
                } else {
                    26
                }
            }
        }
    }

    fn render_value(&self, idx: usize) -> String {
        if let Some((_, v, _)) = self.pending.iter().rev().find(|((mk, sk), _, _)| {
            self.rows
                .get(idx)
                .is_some_and(|r| &r.mod_id == mk && &r.setting.id == sk)
        }) {
            // Format pending raw values like stored ones.
            return Row {
                folder: String::new(),
                mod_id: String::new(),
                setting: crate::spec::Setting {
                    value: Some(v.clone()),
                    ..self.rows[idx].setting.clone()
                },
            }
            .display(&self.strings);
        }
        self.rows[idx].display(&self.strings)
    }

    fn adjust(&mut self, dir: i32) {
        let Some(row) = self.rows.get(self.selected_row()).cloned() else {
            return;
        };
        // Disabled mods render dimmed and ignore value edits; the
        // Mods page owns their switch.
        if !self.mod_on(&row.mod_id) {
            return;
        };
        let new_value: Option<String> = match row.setting.kind {
            SettingKind::Bool => {
                let cur = row.current();
                let on = cur.trim().eq_ignore_ascii_case("true") || cur.trim() == "1";
                Some(if dir == 0 {
                    (!on).to_string()
                } else {
                    on.to_string()
                })
            }
            SettingKind::Enum => {
                let choices = &row.setting.choices;
                if choices.is_empty() {
                    None
                } else {
                    let cur = row.current();
                    let i = choices.iter().position(|c| c == &cur).unwrap_or(0);
                    let n = choices.len() as i32;
                    let next = (i as i32 + dir).rem_euclid(n) as usize;
                    Some(choices[next].clone())
                }
            }
            SettingKind::Float => {
                let cur: f64 = row.current().parse().unwrap_or(0.0);
                let step = row.setting.step.unwrap_or(0.1);
                let mut v = cur + dir as f64 * step;
                if let Some(min) = row.setting.min {
                    v = v.max(min);
                }
                if let Some(max) = row.setting.max {
                    v = v.min(max);
                }
                // Snap to the step grid to avoid float dust, then
                // clamp again in case the grid overshoots max.
                let base = row.setting.min.unwrap_or(0.0);
                v = base + ((v - base) / step).round() * step;
                if let Some(min) = row.setting.min {
                    v = v.max(min);
                }
                if let Some(max) = row.setting.max {
                    v = v.min(max);
                }
                Some(format!("{v:.2}"))
            }
            SettingKind::Int => {
                let cur: i64 = row.current().parse().unwrap_or(0);
                let step = row.setting.step.unwrap_or(1.0) as i64;
                let mut v = cur + dir as i64 * step.max(1);
                if let Some(min) = row.setting.min {
                    v = v.max(min as i64);
                }
                if let Some(max) = row.setting.max {
                    v = v.min(max as i64);
                }
                Some(v.to_string())
            }
            SettingKind::Text => None,
        };
        if let Some(v) = new_value {
            self.commit(row, v);
        }
    }

    /// Row index under the selection, if the selection is a row.
    fn selected_row(&self) -> usize {
        match self.items.get(self.selected) {
            Some(Item::Row(r)) => *r,
            _ => usize::MAX,
        }
    }

    /// Write one setting value to the mod's mod.toml, apply it
    /// immediately (quietly), and remember it for instant display.
    /// Anything the immediate pass does not confirm stays pending
    /// and renders amber until the file-watch reload picks it up.
    fn commit(&mut self, row: Row, value: String) {
        let key = (row.mod_id.clone(), row.setting.id.clone());
        let ok = write_setting_value(&row.folder, &row.setting.id, &row.setting.kind, &value);
        self.tick += 1;
        let tick = self.tick;
        self.pending.retain(|(k, _, _)| k != &key);
        self.pending.push((key, value, tick));
        self.dirty = true;
        if !ok {
            crate::log::append(
                "nozavolochen.log",
                &format!(
                    "overlay: could not write {} in MODS\\{}",
                    row.setting.id, row.folder
                ),
            );
            return;
        }
        if crate::host::poll_reload_quiet() {
            self.rebuild(&crate::locale::localized_mods());
        }
        self.prune_pending();
    }

    /// Drop optimistic entries the reload has confirmed (the live
    /// value now matches what was written).
    fn prune_pending(&mut self) {
        let rows = &self.rows;
        self.pending.retain(|((mk, sk), v, _)| {
            !rows
                .iter()
                .any(|r| &r.mod_id == mk && &r.setting.id == sk && &r.current() == v)
        });
    }

    fn is_pending(&self, idx: usize) -> bool {
        match self.rows.get(idx) {
            Some(row) => self
                .pending
                .iter()
                .any(|((mk, sk), _, _)| mk == &row.mod_id && sk == &row.setting.id),
            None => false,
        }
    }

    /// Whether the mod is switched on, per the last rebuild.
    /// Unknown ids read as on (fail-open: rows stay adjustable).
    fn mod_on(&self, mod_id: &str) -> bool {
        self.groups
            .iter()
            .find(|g| g.id == mod_id)
            .is_none_or(|g| g.enabled)
    }

    /// Mods-page display state: the optimistic switch until the
    /// reload confirms it, else the loaded flag.
    fn mod_display(&self, idx: usize) -> Option<bool> {
        let entry = self.mod_entries.get(idx)?;
        if entry.error.is_some() {
            return None;
        }
        if let Some((_, v, _)) = self
            .pending_mods
            .iter()
            .rev()
            .find(|(id, _, _)| id == &entry.mod_id)
        {
            return Some(*v);
        }
        Some(entry.enabled)
    }

    fn is_mod_pending(&self, idx: usize) -> bool {
        match self.mod_entries.get(idx) {
            Some(entry) => self
                .pending_mods
                .iter()
                .any(|(id, _, _)| id == &entry.mod_id),
            None => false,
        }
    }

    /// Drop optimistic switches the reload has confirmed (the live
    /// value now matches what was written).
    fn prune_pending_mods(&mut self) {
        let entries = &self.mod_entries;
        self.pending_mods
            .retain(|(id, v, _)| !entries.iter().any(|e| &e.mod_id == id && &e.enabled == v));
    }

    /// Flip the selected Mods-page entry's switch.
    fn toggle_mod(&mut self) {
        if let Some(entry) = self.mod_entries.get(self.mod_selected).cloned() {
            if entry.error.is_some() {
                return;
            }
            let next = !self.mod_display(self.mod_selected).unwrap_or(entry.enabled);
            self.commit_mod(entry, next);
        }
    }

    /// Write one mod's `enabled` flag to its mod.toml, apply it live
    /// (quietly), and remember it for instant display. The existing
    /// file-watch reload path retunes every patch of the mod, so
    /// enable and disable both take effect without a restart.
    fn commit_mod(&mut self, entry: ModEntry, value: bool) {
        let ok = write_mod_enabled(&entry.folder, value);
        self.tick += 1;
        let tick = self.tick;
        self.pending_mods.retain(|(id, _, _)| id != &entry.mod_id);
        self.pending_mods.push((entry.mod_id.clone(), value, tick));
        self.dirty = true;
        if !ok {
            crate::log::append(
                "nozavolochen.log",
                &format!("overlay: could not write enabled in MODS\\{}", entry.folder),
            );
            return;
        }
        if crate::host::poll_reload_quiet() {
            self.rebuild(&crate::locale::localized_mods());
        }
        self.prune_pending_mods();
    }

    /// Handle one frame of input. Returns false when the overlay
    /// should close. Keyboard only: the game holds the pointer grab
    /// under Proton, so there is no mouse to speak of.
    fn input(&mut self) -> bool {
        // Escape closes (grave is handled by the toggle).
        if key_edge(self, 0x1B).0 {
            self.editing = None;
            return false;
        }
        // Tab switches pages; Shift+Tab is the Steam overlay hotkey
        // and passes through untouched (see swallow_key). Any text
        // edit is dropped.
        if key_edge(self, VK_TAB).0 && self.editing.is_none() && !shift_down() {
            self.page = match self.page {
                Page::Settings => Page::Mods,
                Page::Mods => Page::Settings,
            };
            self.dirty = true;
            return true;
        }
        if self.page == Page::Mods {
            return self.input_mods();
        }
        if self.editing.is_some() {
            return self.input_text();
        }
        let (up_e, up_r) = key_edge(self, 0x26);
        let (dn_e, dn_r) = key_edge(self, 0x28);
        if (up_e || up_r) && self.selected > 0 {
            self.selected -= 1;
            self.dirty = true;
        }
        if (dn_e || dn_r) && self.selected + 1 < self.items.len() {
            self.selected += 1;
            self.dirty = true;
        }
        let (lf_e, lf_r) = key_edge(self, 0x25);
        let (rt_e, rt_r) = key_edge(self, 0x27);
        if lf_e || lf_r {
            self.on_side(-1);
        }
        if rt_e || rt_r {
            self.on_side(1);
        }
        if key_edge(self, 0x0D).0 {
            match self.items.get(self.selected) {
                Some(Item::Header(g)) => {
                    let g = *g;
                    self.toggle_group(g);
                }
                Some(Item::Row(_)) => {
                    if let Some(row) = self.selected_row_item() {
                        // Disabled mods ignore value edits (adjust
                        // already guards bool/enum/float/int).
                        if !self.mod_on(&row.mod_id) {
                            return true;
                        }
                        match row.setting.kind {
                            SettingKind::Bool => self.adjust(0),
                            SettingKind::Enum => self.adjust(1),
                            SettingKind::Text => {
                                self.editing = Some((self.selected_row(), row.current()));
                                self.dirty = true;
                            }
                            _ => {}
                        }
                    }
                }
                None => {}
            }
        }
        // Delete resets the selection to its default. Rows of a
        // disabled mod ignore it (their values are frozen while off).
        if key_edge(self, 0x2E).0 {
            match self.items.get(self.selected) {
                Some(Item::Header(g)) => {
                    let rows: Vec<Row> = self
                        .rows
                        .iter()
                        .filter(|r| self.groups.get(*g).is_some_and(|gr| gr.id == r.mod_id))
                        .cloned()
                        .collect();
                    for row in rows {
                        if self.mod_on(&row.mod_id) {
                            self.reset_row(&row);
                        }
                    }
                }
                Some(Item::Row(_)) => {
                    if let Some(row) = self.selected_row_item() {
                        if self.mod_on(&row.mod_id) {
                            self.reset_row(&row);
                        }
                    }
                }
                None => {}
            }
        }
        true
    }

    /// Handle one frame of input on the Mods page. Returns false
    /// when the overlay should close.
    fn input_mods(&mut self) -> bool {
        let (up_e, up_r) = key_edge(self, 0x26);
        let (dn_e, dn_r) = key_edge(self, 0x28);
        if (up_e || up_r) && self.mod_selected > 0 {
            self.mod_selected -= 1;
            self.dirty = true;
        }
        if (dn_e || dn_r) && self.mod_selected + 1 < self.mod_entries.len() {
            self.mod_selected += 1;
            self.dirty = true;
        }
        let (lf_e, lf_r) = key_edge(self, 0x25);
        let (rt_e, rt_r) = key_edge(self, 0x27);
        if lf_e || lf_r {
            self.toggle_mod();
        }
        if rt_e || rt_r {
            self.toggle_mod();
        }
        if key_edge(self, 0x0D).0 {
            self.toggle_mod();
        }
        // Delete resets the mod to enabled.
        if key_edge(self, 0x2E).0 {
            if let Some(entry) = self.mod_entries.get(self.mod_selected).cloned() {
                if entry.error.is_none() && !entry.enabled {
                    self.commit_mod(entry, true);
                }
            }
        }
        true
    }

    fn selected_row_item(&self) -> Option<Row> {
        match self.items.get(self.selected) {
            Some(Item::Row(r)) => self.rows.get(*r).cloned(),
            _ => None,
        }
    }

    /// Reset one row to its default by dropping its `value` line, so
    /// the default shows again.
    fn reset_row(&mut self, row: &Row) {
        if clear_setting_value(&row.folder, &row.setting.id) {
            self.pending
                .retain(|(k, _, _)| k != &(row.mod_id.clone(), row.setting.id.clone()));
            self.dirty = true;
        }
    }

    /// Left/right on the selection: adjust rows, collapse headers.
    fn on_side(&mut self, dir: i32) {
        match self.items.get(self.selected) {
            Some(Item::Header(g)) => {
                let g = *g;
                self.toggle_group(g);
            }
            Some(Item::Row(_)) => self.adjust(dir),
            None => {}
        }
    }

    fn input_text(&mut self) -> bool {
        // Backspace.
        if key_edge(self, 0x08).0 {
            if let Some((_, buf)) = self.editing.as_mut() {
                buf.pop();
                self.dirty = true;
            }
        }
        // Enter commits, Escape cancels (handled above).
        if key_edge(self, 0x0D).0 {
            if let Some((i, buf)) = self.editing.take() {
                if let Some(row) = self.rows.get(i).cloned() {
                    self.commit(row, buf);
                }
            }
            return true;
        }
        // Printable characters.
        let shift = shift_down();
        for vk in 0x41..=0x5A {
            if key_edge(self, vk).0 {
                if let Some((_, buf)) = self.editing.as_mut() {
                    let c = (vk as u8) as char;
                    buf.push(if shift { c } else { c.to_ascii_lowercase() });
                    self.dirty = true;
                }
            }
        }
        for vk in 0x30..=0x39 {
            if key_edge(self, vk).0 {
                if let Some((_, buf)) = self.editing.as_mut() {
                    let d = (vk as u8 - 0x30) as char;
                    const SHIFTED: [char; 10] = ['(', '!', '@', '#', '$', '%', '^', '&', '*', ')'];
                    buf.push(if shift {
                        SHIFTED[d as usize - '0' as usize]
                    } else {
                        d
                    });
                    self.dirty = true;
                }
            }
        }
        if key_edge(self, 0x20).0 {
            if let Some((_, buf)) = self.editing.as_mut() {
                buf.push(' ');
                self.dirty = true;
            }
        }
        if key_edge(self, 0xBD).0 {
            if let Some((_, buf)) = self.editing.as_mut() {
                buf.push(if shift { '_' } else { '-' });
                self.dirty = true;
            }
        }
        if key_edge(self, 0xBE).0 {
            if let Some((_, buf)) = self.editing.as_mut() {
                buf.push(if shift { '>' } else { '.' });
                self.dirty = true;
            }
        }
        true
    }
}
/// Drop one setting's `value` line from the mod's mod.toml, so the
/// default applies again. Returns false when there was no value line.
fn clear_setting_value(folder: &str, setting_id: &str) -> bool {
    let Some(dir) = crate::host::mods_dir() else {
        return false;
    };
    let sep = if dir.contains('\\') { "\\" } else { "/" };
    let path = format!("{dir}{sep}{folder}{sep}mod.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let mut out: Vec<String> = Vec::new();
    let mut in_setting = false;
    let mut block_id: Option<String> = None;
    let mut removed = false;
    for raw in text.lines() {
        let stripped = raw.split(['#', ';']).next().unwrap_or("").trim();
        if stripped.starts_with('[') {
            in_setting = stripped.eq_ignore_ascii_case("[[setting]]");
            block_id = None;
            out.push(raw.to_string());
            continue;
        }
        if in_setting {
            if let Some((k, v)) = stripped.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                if key == "id" {
                    block_id = Some(v.trim().trim_matches('"').to_string());
                } else if key == "value" && block_id.as_deref() == Some(setting_id) {
                    removed = true;
                    continue;
                }
            }
        }
        out.push(raw.to_string());
    }
    if !removed {
        return false;
    }
    std::fs::write(&path, out.join("\n")).is_ok()
}
/// Rewrite one setting's `value` line in the mod's mod.toml,
/// preserving every other byte. Inserts the line when absent.
fn write_setting_value(folder: &str, setting_id: &str, kind: &SettingKind, value: &str) -> bool {
    use crate::spec::SettingKind as K;
    let Some(dir) = crate::host::mods_dir() else {
        return false;
    };
    let sep = if dir.contains('\\') { "\\" } else { "/" };
    let path = format!("{dir}{sep}{folder}{sep}mod.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let rendered = match kind {
        K::Bool => {
            let on = value.trim().eq_ignore_ascii_case("true") || value.trim() == "1";
            if on {
                "true".into()
            } else {
                "false".into()
            }
        }
        K::Float | K::Int => value.trim().to_string(),
        K::Enum | K::Text => format!("\"{}\"", value.replace('"', "")),
    };
    let mut out: Vec<String> = Vec::new();
    let mut in_setting = false;
    let mut block_id: Option<String> = None;
    let mut value_line: Option<usize> = None;
    let mut done = false;
    for raw in text.lines() {
        let stripped = raw.split(['#', ';']).next().unwrap_or("").trim();
        if stripped.starts_with('[') {
            // Close the previous [[setting]] block.
            if in_setting && block_id.as_deref() == Some(setting_id) && !done {
                if let Some(vl) = value_line {
                    out[vl] = format!("value = {rendered}");
                } else {
                    out.push(format!("value = {rendered}"));
                }
                done = true;
            }
            in_setting = stripped.eq_ignore_ascii_case("[[setting]]");
            block_id = None;
            value_line = None;
            out.push(raw.to_string());
            continue;
        }
        if in_setting {
            if let Some((k, v)) = stripped.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                if key == "id" {
                    block_id = Some(v.trim().trim_matches('"').to_string());
                } else if key == "value" {
                    value_line = Some(out.len());
                }
            }
        }
        out.push(raw.to_string());
    }
    // Trailing block.
    if in_setting && block_id.as_deref() == Some(setting_id) && !done {
        if let Some(vl) = value_line {
            out[vl] = format!("value = {rendered}");
        } else {
            out.push(format!("value = {rendered}"));
        }
        done = true;
    }
    if !done {
        return false;
    }
    std::fs::write(&path, out.join("\n")).is_ok()
}

/// Pure rewrite of the `[mod] enabled` line for tests and the
/// overlay switch. Preserves every other byte; inserts the line
/// when absent. Returns None when there is no `[mod]` section.
fn rewrite_enabled_in_text(text: &str, enabled: bool) -> Option<String> {
    let rendered = if enabled { "true" } else { "false" };
    let mut out: Vec<String> = Vec::new();
    let mut in_mod = false;
    let mut value_line: Option<usize> = None;
    let mut done = false;
    for raw in text.lines() {
        let stripped = raw.split(['#', ';']).next().unwrap_or("").trim();
        if stripped.starts_with('[') {
            // Close the [mod] section.
            if in_mod && !done {
                if let Some(vl) = value_line {
                    out[vl] = format!("enabled = {rendered}");
                } else {
                    out.push(format!("enabled = {rendered}"));
                }
                done = true;
            }
            in_mod = stripped.eq_ignore_ascii_case("[mod]");
            value_line = None;
            out.push(raw.to_string());
            continue;
        }
        if in_mod {
            if let Some((k, _)) = stripped.split_once('=') {
                if k.trim().eq_ignore_ascii_case("enabled") {
                    value_line = Some(out.len());
                }
            }
        }
        out.push(raw.to_string());
    }
    // Trailing [mod] section.
    if in_mod && !done {
        if let Some(vl) = value_line {
            out[vl] = format!("enabled = {rendered}");
        } else {
            out.push(format!("enabled = {rendered}"));
        }
        done = true;
    }
    done.then(|| out.join("\n"))
}

/// Rewrite one mod's `[mod] enabled` line in its mod.toml,
/// preserving every other byte. Inserts the line when absent.
fn write_mod_enabled(folder: &str, enabled: bool) -> bool {
    let Some(dir) = crate::host::mods_dir() else {
        return false;
    };
    let sep = if dir.contains('\\') { "\\" } else { "/" };
    let path = format!("{dir}{sep}{folder}{sep}mod.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Some(next) = rewrite_enabled_in_text(&text, enabled) else {
        return false;
    };
    std::fs::write(&path, next).is_ok()
}

type PresentFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type PeekFn = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> i32;

/// Hook health beacons only. All input and polling runs on the
/// dedicated input thread, so quick taps can never be eaten by an
/// irregular hook rate.
unsafe extern "system" fn present_hook(queue: *mut c_void, info: *mut c_void) -> i32 {
    let original: PresentFn = std::mem::transmute(PRESENT_ORIG);
    original(queue, info)
}

unsafe extern "system" fn peek_hook(
    msg: *mut c_void,
    hwnd: *mut c_void,
    min: u32,
    max: u32,
    remove: u32,
) -> i32 {
    let original: PeekFn = std::mem::transmute(PUMP_ORIG);
    original(msg, hwnd, min, max, remove)
}

/// Dedicated input thread: fixed-rate polling independent of hook
/// timing, with a message pump so the key hook fires. Started lazily
/// on first use (never inside DllMain).
unsafe extern "system" fn thread_main(_: *mut c_void) -> u32 {
    install_key_hook();
    loop {
        pump_messages();
        steam_watch_poll();
        poll_toggle();
        maybe_poll_reload();
        if OPEN.load(Ordering::Relaxed) {
            frame();
        } else if let Ok(mut st) = ui().try_lock() {
            st.hide();
        }
    }
}

/// Wait for window messages up to the poll interval, then dispatch
/// them. The low-level key hook needs this pump on its thread.
fn pump_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageA, MsgWaitForMultipleObjects, PeekMessageA, TranslateMessage, MSG,
        PM_REMOVE, QS_ALLINPUT,
    };
    unsafe {
        // WAIT_OBJECT_0 (0): messages waiting; anything else (such as
        // WAIT_TIMEOUT): interval elapsed, poll anyway.
        if MsgWaitForMultipleObjects(0, std::ptr::null(), 0, INPUT_MS, QS_ALLINPUT) != 0 {
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageA(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }
    }
}

/// Swallow overlay keys while open so the game does not hear them.
/// The poll still sees physical state, so capture never breaks the
/// toggle or navigation: it only silences the game underneath.
unsafe extern "system" fn kb_hook(
    ncode: i32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };
    unsafe {
        if ncode as u32 == HC_ACTION {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            // Feed polling first: swallowed keys are invisible to
            // GetAsyncKeyState.
            if (kb.vkCode as usize) < 256 {
                let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
                let up = wparam as u32 == WM_KEYUP || wparam as u32 == WM_SYSKEYUP;
                if down {
                    HOOK_DOWN[kb.vkCode as usize].store(true, Ordering::Relaxed);
                } else if up {
                    HOOK_DOWN[kb.vkCode as usize].store(false, Ordering::Relaxed);
                }
            }
            // The toggle follows the physical key, so record its scan
            // code alongside the layout-dependent VK above.
            if is_toggle_scancode(kb.scanCode) {
                let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
                let up = wparam as u32 == WM_KEYUP || wparam as u32 == WM_SYSKEYUP;
                if down {
                    SC_GRAVE_DOWN.store(true, Ordering::Relaxed);
                } else if up {
                    SC_GRAVE_DOWN.store(false, Ordering::Relaxed);
                }
            }
            if wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN {
                if swallow_key(kb.vkCode, kb.scanCode) {
                    return 1;
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam)
    }
}

/// Whether the game should not see this key right now: overlay keys
/// while open (plus printable keys in text entry), and only when the
/// game client itself is foreground so other apps (and Steam overlay
/// chats) are never affected. While the Steam overlay is up nothing
/// is swallowed at all: muted like the game.
fn swallow_key(vk: u32, sc: u32) -> bool {
    if !OPEN.load(Ordering::Relaxed) {
        return false;
    }
    if steam_overlay_open() {
        return false;
    }
    if foreground_window().is_none() {
        return false;
    }
    // The toggle follows the physical key: swallow its scan code on
    // every layout. The VK entry below stays as fallback for hosts
    // whose hook reports no scan code.
    if is_toggle_scancode(sc) {
        return true;
    }
    match vk {
        // Navigation, toggle, reset, page switch, close. Tab only
        // without Shift: Shift+Tab passes through so the Steam
        // overlay hotkey keeps working while we are open.
        0x25..=0x28 | 0x0D | 0x1B | 0x2E | 0xC0 => true,
        0x09 => !shift_down(),
        // Text entry extras.
        0x08 | 0x20 | 0xBD | 0xBE => EDITING.load(Ordering::Relaxed),
        0x30..=0x39 | 0x41..=0x5A => EDITING.load(Ordering::Relaxed),
        _ => false,
    }
}

fn install_key_hook() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowsHookExA, WH_KEYBOARD_LL};
    unsafe {
        let h = SetWindowsHookExA(WH_KEYBOARD_LL, Some(kb_hook), std::ptr::null_mut(), 0);
        HOOK = h as isize;
        if HOOK == 0 {
            crate::log::append("nozavolochen.log", "overlay: key hook NOT INSTALLED");
        }
    }
}

fn ensure_thread() {
    THREAD_STARTED.get_or_init(|| unsafe {
        use windows_sys::Win32::System::Threading::CreateThread;
        let h = CreateThread(
            std::ptr::null(),
            0,
            Some(thread_main),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        );
        if h.is_null() {
            crate::log::append("nozavolochen.log", "overlay: input thread failed to start");
        }
    });
}

fn poll_toggle() {
    let down = toggle_down();
    unsafe {
        let was = WAS_DOWN;
        WAS_DOWN = down;
        if down && !was {
            if toggle_allowed(steam_overlay_open(), foreground_window().is_some()) {
                let now_open = !OPEN.load(Ordering::Relaxed);
                OPEN.store(now_open, Ordering::Relaxed);
                if now_open {
                    if let Ok(mut st) = ui().try_lock() {
                        st.open();
                    }
                } else {
                    clear_hook_keys();
                }
            } else {
                WAS_DOWN = true;
            }
        }
    }
}

/// Re-check mod.toml mtimes every couple of seconds and rebuild the
/// row list when anything reloaded. The tick is cheap (one timer
/// read); the filesystem is only touched when the interval elapsed.
fn maybe_poll_reload() {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    unsafe {
        let now = GetTickCount64();
        if now.wrapping_sub(LAST_POLL_MS) >= RELOAD_MS {
            LAST_POLL_MS = now;
            if crate::host::poll_reload() {
                if let Ok(mut st) = ui().try_lock() {
                    st.rebuild(&crate::locale::localized_mods());
                }
            }
        }
    }
}

/// One open frame: input, window sync, repaint when dirty.
fn frame() {
    if steam_overlay_open() {
        // Steam overlay up: close rather than sit on top of it.
        OPEN.store(false, Ordering::Relaxed);
        clear_hook_keys();
        if let Ok(mut st) = ui().try_lock() {
            st.hide();
        }
        return;
    }
    let game = match foreground_window() {
        Some(h) => h,
        None => {
            // Game client lost focus (alt-tab, or a Steam overlay chat
            // in front): close rather than linger.
            OPEN.store(false, Ordering::Relaxed);
            clear_hook_keys();
            if let Ok(mut st) = ui().try_lock() {
                st.hide();
            }
            return;
        }
    };
    let _ = match ui().try_lock() {
        Ok(mut st) => {
            if !st.input() {
                OPEN.store(false, Ordering::Relaxed);
                clear_hook_keys();
                EDITING.store(false, Ordering::Relaxed);
                st.hide();
                false
            } else {
                EDITING.store(st.editing.is_some(), Ordering::Relaxed);
                st.sync_window(game);
                if st.dirty {
                    st.paint();
                    st.dirty = false;
                }
                true
            }
        }
        Err(_) => true,
    };
}

// ---------------------------------------------------------------------------
// Win32 panel: opaque topmost tool window, GDI-painted on change.
// ---------------------------------------------------------------------------

impl Overlay {
    fn ensure_window(&mut self) -> bool {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        if self.hwnd != 0 {
            return true;
        }
        unsafe {
            if !self.class_registered {
                let cls_name = b"nozavolochen_overlay\0";
                let wc = WNDCLASSA {
                    style: 0,
                    lpfnWndProc: Some(wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: std::ptr::null_mut(),
                    hIcon: std::ptr::null_mut(),
                    hCursor: std::ptr::null_mut(),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: cls_name.as_ptr(),
                };
                if RegisterClassA(&wc) != 0 {
                    self.class_registered = true;
                } else {
                    crate::log::append(
                        "nozavolochen.log",
                        &format!("overlay: RegisterClassA failed (error {})", GetLastError()),
                    );
                }
            }
            if !self.class_registered {
                return false;
            }
            let hwnd = CreateWindowExA(
                (WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as u32,
                b"nozavolochen_overlay\0".as_ptr(),
                b"nozavolochen\0".as_ptr(),
                WS_POPUP,
                0,
                0,
                8,
                8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                crate::log::append(
                    "nozavolochen.log",
                    &format!("overlay: CreateWindowExA failed (error {})", GetLastError()),
                );
                return false;
            }
            self.hwnd = hwnd as isize;
            true
        }
    }

    fn open(&mut self) {
        self.window_dead = false;
        if !self.ensure_window() {
            self.window_dead = true;
            return;
        }
        self.game_hwnd = 0;
        self.editing = None;
        self.selected = 0;
        self.mod_selected = 0;
        self.page = Page::Settings;
        // Cheap (two small file reads): NMS hot-applies language
        // changes, so re-resolve on every open. Chrome strings and
        // mod text localise together from the same language.
        let (lang, source) = crate::locale::refresh();
        crate::log::append(
            "nozavolochen.log",
            &format!("overlay: language {lang} ({source})"),
        );
        self.strings = Strings::load(&lang, None);
        self.rebuild(&crate::locale::localized_mods());
        self.sync_window(foreground_window().unwrap_or(0));
        self.dirty = true;
        self.paint();
        self.dirty = false;
        self.show(true);
    }

    fn hide(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow;
        unsafe {
            if self.hwnd != 0 {
                ShowWindow(self.hwnd as _, 0); // SW_HIDE
            }
        }
    }

    fn show(&mut self, visible: bool) {
        use windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow;
        unsafe {
            if self.hwnd != 0 {
                ShowWindow(self.hwnd as _, if visible { 8 } else { 0 }); // SW_SHOWNA
            }
        }
    }

    /// Pin the panel to the game client area, above the game window.
    fn sync_window(&mut self, game: isize) {
        use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
        use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetClientRect, IsIconic, SetWindowPos, HWND_TOPMOST,
        };
        if self.window_dead {
            return;
        }
        unsafe {
            if game == 0 {
                return;
            }
            self.game_hwnd = game;
            if IsIconic(game as HWND) != 0 {
                self.hide();
                return;
            }
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetClientRect(game as HWND, &mut rc) == 0 {
                return;
            }
            let mut pt = POINT {
                x: rc.left,
                y: rc.top,
            };
            ClientToScreen(game as HWND, &mut pt);
            self.ensure_window();
            if self.hwnd == 0 {
                return;
            }
            let w = self.panel_w();
            let h = self.panel_h();
            let x = pt.x + 24;
            let y = pt.y + 24;
            SetWindowPos(
                self.hwnd as HWND,
                HWND_TOPMOST,
                x,
                y,
                w,
                h,
                0x0010 | 0x0040, // SWP_NOACTIVATE | SWP_SHOWWINDOW
            );
            self.show(true);
        }
    }

    fn panel_w(&self) -> i32 {
        620
    }

    /// One footer line of (key, action) pairs: keys amber, actions gray.
    fn draw_footer_line(&self, hdc: *mut c_void, y: i32, pairs: &[(&str, &str)]) {
        use windows_sys::Win32::Foundation::{RECT, SIZE};
        use windows_sys::Win32::Graphics::Gdi::{DrawTextW, GetTextExtentPoint32W, SetTextColor};
        unsafe {
            let mut x = 16;
            for (key, action) in pairs {
                let w = self.panel_w();
                SetTextColor(hdc, 0x0082CDFF);
                let mut krc = RECT {
                    left: x,
                    top: y,
                    right: w - 16,
                    bottom: y + 22,
                };
                let ktext = wide(key);
                DrawTextW(
                    hdc,
                    ktext.as_ptr(),
                    -1,
                    &mut krc,
                    0x00000000 | 0x00000020 | 0x00000800,
                );
                let mut ext = SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32W(hdc, ktext.as_ptr(), (ktext.len() - 1) as i32, &mut ext);
                x += ext.cx;
                SetTextColor(hdc, 0x00909090);
                let mut arc = RECT {
                    left: x,
                    top: y,
                    right: w - 16,
                    bottom: y + 22,
                };
                let atext = wide(action);
                DrawTextW(
                    hdc,
                    atext.as_ptr(),
                    -1,
                    &mut arc,
                    0x00000000 | 0x00000020 | 0x00000800,
                );
                GetTextExtentPoint32W(hdc, atext.as_ptr(), (atext.len() - 1) as i32, &mut ext);
                x += ext.cx;
            }
        }
    }

    fn panel_h(&self) -> i32 {
        136 + if self.page == Page::Mods {
            self.mod_entries
                .iter()
                .map(|e| self.mod_entry_height(e))
                .sum::<i32>()
        } else {
            self.items
                .iter()
                .map(|it| self.item_height(it))
                .sum::<i32>()
        }
    }

    /// Height of one Mods-page entry: entries with a description or
    /// a parse error take a second line for the byline.
    fn mod_entry_height(&self, entry: &ModEntry) -> i32 {
        if !entry.description.is_empty() || entry.error.is_some() {
            44
        } else {
            26
        }
    }

    /// Paint the whole panel with GDI. Called on open and whenever
    /// the content changes, never per frame.
    fn paint(&mut self) {
        use windows_sys::Win32::Foundation::{HWND, RECT, SIZE};
        use windows_sys::Win32::Graphics::Gdi::*;
        if self.window_dead {
            return;
        }
        unsafe {
            if self.hwnd == 0 {
                return;
            }
            let w = self.panel_w();
            let h = self.panel_h();
            let hdc = GetDC(self.hwnd as HWND);
            if hdc.is_null() {
                return;
            }
            let mkfont = |weight: i32| {
                let face = wide("Consolas");
                CreateFontW(
                    -16,
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    1, // DEFAULT_CHARSET
                    0,
                    0,
                    0,
                    0,
                    face.as_ptr(),
                )
            };
            let regular = mkfont(400);
            let bold = mkfont(700);
            SelectObject(hdc, regular);
            SetBkMode(hdc, 1); // TRANSPARENT
                               // Background.
            let bg = CreateSolidBrush(0x16100E); // BGR: rgb(14,16,22)
            let full = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(hdc, &full, bg);
            DeleteObject(bg);
            // Header.
            SelectObject(hdc, bold);
            SetTextColor(hdc, 0x0082CDFF); // amber (255,205,130)
            let mut rc = RECT {
                left: 16,
                top: 12,
                right: w - 16,
                bottom: 38,
            };
            DrawTextW(
                hdc,
                wide("Nozavolochen").as_ptr(),
                -1,
                &mut rc,
                0x00000000 | 0x00000020 | 0x00000800,
            );
            // Amber rule under the header.
            let rule = CreateSolidBrush(0x0082CDFF);
            let rulerect = RECT {
                left: 16,
                top: 42,
                right: w - 16,
                bottom: 44,
            };
            FillRect(hdc, &rulerect, rule);
            DeleteObject(rule);
            // Page tabs, right-aligned: the active page in brackets.
            // Page names stay English (like key labels); only the
            // footer actions translate.
            let tabs = match self.page {
                Page::Settings => "[Settings]  Mods",
                Page::Mods => "Settings  [Mods]",
            };
            let tabw = wide(tabs);
            let mut tabsz = SIZE { cx: 0, cy: 0 };
            GetTextExtentPoint32W(hdc, tabw.as_ptr(), (tabw.len() - 1) as i32, &mut tabsz);
            let mut tbrc = RECT {
                left: w - 16 - tabsz.cx,
                top: 12,
                right: w - 16,
                bottom: 38,
            };
            DrawTextW(
                hdc,
                tabw.as_ptr(),
                -1,
                &mut tbrc,
                0x00000000 | 0x00000020 | 0x00000800,
            );
            SelectObject(hdc, regular);
            // Mods page: one row per installed mod, then the shared footer.
            let mut y = 52;
            if self.page == Page::Mods {
                for n in 0..self.mod_entries.len() {
                    let entry = self.mod_entries[n].clone();
                    let tall = !entry.description.is_empty() || entry.error.is_some();
                    let h = if tall { 44 } else { 26 };
                    // No selection fill: like Settings-page headers,
                    // the selected entry shows `> ` plus white text.
                    let marker = if n == self.mod_selected { "> " } else { "  " };
                    SelectObject(hdc, bold);
                    SetTextColor(
                        hdc,
                        if n == self.mod_selected {
                            0x00FFFFFF
                        } else {
                            0x00CCCCCC
                        },
                    );
                    let mut mrc = RECT {
                        left: 16,
                        top: y,
                        right: w - 16,
                        bottom: y + 26,
                    };
                    let title = if entry.version.is_empty() {
                        format!("{marker}{}", entry.label)
                    } else {
                        format!("{marker}{} {}", entry.label, entry.version)
                    };
                    DrawTextW(
                        hdc,
                        wide(&title).as_ptr(),
                        -1,
                        &mut mrc,
                        0x00000000 | 0x00000020 | 0x00000800,
                    );
                    SelectObject(hdc, regular);
                    // State right-aligned: on/off, or error for
                    // unloadable folders. Pending switches render amber.
                    let state: &str = match self.mod_display(n) {
                        None => "error",
                        Some(true) => self.strings.get("chrome.on"),
                        Some(false) => self.strings.get("chrome.off"),
                    };
                    let pending = self.is_mod_pending(n);
                    SetTextColor(
                        hdc,
                        if entry.error.is_some() || pending {
                            0x0082CDFF
                        } else if n == self.mod_selected {
                            0x00FFFFFF
                        } else {
                            0x00CCCCCC
                        },
                    );
                    let mut ext = SIZE { cx: 0, cy: 0 };
                    let statew = wide(state);
                    GetTextExtentPoint32W(
                        hdc,
                        statew.as_ptr(),
                        (statew.len() - 1) as i32,
                        &mut ext,
                    );
                    let mut vrc = RECT {
                        left: w - 20 - ext.cx,
                        top: y,
                        right: w - 20,
                        bottom: y + 26,
                    };
                    DrawTextW(
                        hdc,
                        wide(state).as_ptr(),
                        -1,
                        &mut vrc,
                        0x00000000 | 0x00000020 | 0x00000800,
                    );
                    if tall {
                        // Explainer or parse error under the name;
                        // errors render amber so they stand out.
                        SetTextColor(
                            hdc,
                            if entry.error.is_some() {
                                0x0082CDFF
                            } else {
                                0x00909090
                            },
                        );
                        let mut drc = RECT {
                            left: 52,
                            top: y + 22,
                            right: w - 16,
                            bottom: y + 44,
                        };
                        let byline = entry.error.as_deref().unwrap_or(&entry.description);
                        // Truncate parse errors to one line.
                        let byline: String = byline
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(90)
                            .collect();
                        DrawTextW(
                            hdc,
                            wide(&byline).as_ptr(),
                            -1,
                            &mut drc,
                            0x00000000 | 0x00000020 | 0x00000800,
                        );
                    }
                    y += h;
                }
            } else {
                // Groups and rows.
                for (n, item) in self.items.iter().enumerate() {
                    match item {
                        Item::Header(g) => {
                            let group = &self.groups[*g];
                            SelectObject(hdc, bold);
                            SetTextColor(hdc, 0x00FFFFFF);
                            let mut mrc = RECT {
                                left: 16,
                                top: y,
                                right: w - 16,
                                bottom: y + 26,
                            };
                            let marker = if group.collapsed { "[+]" } else { "[-]" };
                            let text = wide(&format!(
                                "{}{} {}",
                                if n == self.selected { "> " } else { "  " },
                                marker,
                                group.label
                            ));
                            DrawTextW(
                                hdc,
                                text.as_ptr(),
                                -1,
                                &mut mrc,
                                0x00000000 | 0x00000020 | 0x00000800,
                            );
                            SelectObject(hdc, regular);
                            if group.description.is_empty() {
                                y += 26;
                            } else {
                                // Byline under the mod name, dim gray.
                                SetTextColor(hdc, 0x00909090);
                                let mut drc = RECT {
                                    left: 52,
                                    top: y + 22,
                                    right: w - 16,
                                    bottom: y + 44,
                                };
                                let dtext = wide(&group.description);
                                DrawTextW(
                                    hdc,
                                    dtext.as_ptr(),
                                    -1,
                                    &mut drc,
                                    0x00000000 | 0x00000020 | 0x00000800,
                                );
                                y += 44;
                            }
                        }
                        Item::Row(r) => {
                            let row = self.rows[*r].clone();
                            let tall = !row.setting.description.is_empty();
                            let h = if tall { 44 } else { 26 };
                            if n == self.selected {
                                let sel = CreateSolidBrush(0x005A463C); // BGR (60,70,90)
                                let srect = RECT {
                                    left: 8,
                                    top: y,
                                    right: w - 8,
                                    bottom: y + h,
                                };
                                FillRect(hdc, &srect, sel);
                                DeleteObject(sel);
                            }
                            let marker = if n == self.selected { "> " } else { "  " };
                            let val = match &self.editing {
                                Some((e, buf)) if *e == *r => format!("{buf}_"),
                                _ => self.render_value(*r),
                            };
                            // Pending (unconfirmed) values render amber; rows
                            // of a disabled mod render dimmed; the rest
                            // follow the selection.
                            let pending = self.is_pending(*r);
                            let live = self.mod_on(&row.mod_id);
                            SetTextColor(
                                hdc,
                                if pending {
                                    0x0082CDFF
                                } else if !live {
                                    0x00909090
                                } else if n == self.selected {
                                    0x00FFFFFF
                                } else {
                                    0x00CCCCCC
                                },
                            );
                            let mut rrc = RECT {
                                left: 28,
                                top: y,
                                right: w - 20,
                                bottom: y + 26,
                            };
                            let label_text = wide(&format!("{}{}:", marker, row.setting.label));
                            DrawTextW(
                                hdc,
                                label_text.as_ptr(),
                                -1,
                                &mut rrc,
                                0x00000000 | 0x00000020 | 0x00000800,
                            );
                            // Value right-aligned.
                            let mut ext = SIZE { cx: 0, cy: 0 };
                            let valw = wide(&val);
                            GetTextExtentPoint32W(
                                hdc,
                                valw.as_ptr(),
                                (valw.len() - 1) as i32,
                                &mut ext,
                            );
                            let mut vrc = RECT {
                                left: w - 20 - ext.cx,
                                top: y,
                                right: w - 20,
                                bottom: y + 26,
                            };
                            let vtext = wide(&val);
                            DrawTextW(
                                hdc,
                                vtext.as_ptr(),
                                -1,
                                &mut vrc,
                                0x00000000 | 0x00000020 | 0x00000800,
                            );
                            if tall {
                                // Explainer under the setting, dim gray.
                                SetTextColor(hdc, 0x00909090);
                                let mut drc = RECT {
                                    left: 52,
                                    top: y + 22,
                                    right: w - 16,
                                    bottom: y + 44,
                                };
                                let dtext = wide(&row.setting.description);
                                DrawTextW(
                                    hdc,
                                    dtext.as_ptr(),
                                    -1,
                                    &mut drc,
                                    0x00000000 | 0x00000020 | 0x00000800,
                                );
                            }
                            y += h;
                        }
                    }
                }
            } // Page::Settings items loop
              // Footer: keys amber, actions gray. Key names are keyboard
              // labels and stay untranslated; actions come from strings.
            let s = &self.strings;
            self.draw_footer_line(
                hdc,
                h - 56,
                &[
                    ("up/down", &format!(" {}  ", s.get("chrome.select"))),
                    ("left/right", &format!(" {}  ", s.get("chrome.adjust"))),
                    ("enter", &format!(" {}  ", s.get("chrome.ok"))),
                    ("del", &format!(" {}  ", s.get("chrome.reset"))),
                    ("esc", &format!(" {}", s.get("chrome.close"))),
                ],
            );
            self.draw_footer_line(
                hdc,
                h - 30,
                &[
                    ("tab", &format!(" {}  ", s.get("chrome.switch"))),
                    ("`", &format!(" {}", s.get("chrome.close"))),
                ],
            );
            DeleteObject(regular);
            DeleteObject(bold);
            ReleaseDC(self.hwnd as HWND, hdc);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcA;
    unsafe { DefWindowProcA(hwnd, msg, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Host wiring.
// ---------------------------------------------------------------------------

/// Read a PE string table entry as an ASCII lowercase name.
fn dll_name_matches(rva_name: usize, base: usize, want: &str) -> bool {
    unsafe {
        let mut bytes = Vec::new();
        let mut p = (base + rva_name) as *const u8;
        for _ in 0..64 {
            let b = *p;
            if b == 0 {
                break;
            }
            bytes.push(b.to_ascii_lowercase());
            p = p.add(1);
        }
        bytes == want.as_bytes()
    }
}

fn import_dir(base: usize) -> Option<(usize, usize)> {
    unsafe {
        let b = base as *const u8;
        if *(b as *const u16) != 0x5A4D {
            return None;
        }
        let lfanew = *(b.add(0x3C) as *const u32) as usize;
        let magic = *(b.add(lfanew + 24) as *const u16);
        if magic != 0x20B {
            return None;
        }
        // PE32+ data directories start at lfanew+24+112 (NOT +96:
        // that is the 32-bit layout); import dir is index 1.
        let dd = b.add(lfanew + 24 + 112 + 8);
        let rva = *(dd as *const u32) as usize;
        let size = *(dd.add(4) as *const u32) as usize;
        if rva == 0 || size == 0 {
            return None;
        }
        Some((base + rva, size))
    }
}

/// Find the host IAT slot resolving to `expected` inside imports of
/// `dll`, and swap in `hook`. Returns the slot's RVA on success.
fn swap_iat(dll: &str, expected: usize, hook: usize) -> Result<usize, String> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleA;
    use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_READWRITE};
    unsafe {
        let base = GetModuleHandleA(std::ptr::null()) as usize;
        if base == 0 {
            return Err("host base unknown".into());
        }
        let (dir, size) = import_dir(base).ok_or("host import directory not found")?;
        // IMAGE_IMPORT_DESCRIPTOR is 20 bytes; ends with a null entry.
        let mut d = dir;
        let end = dir + size;
        while d + 20 <= end {
            let name_rva = *(d as *const u32).byte_add(12) as usize;
            let first_thunk = *(d as *const u32).byte_add(16) as usize;
            if name_rva == 0 && first_thunk == 0 {
                break;
            }
            if name_rva != 0 && first_thunk != 0 && dll_name_matches(name_rva, base, dll) {
                let mut slot = base + first_thunk;
                loop {
                    let cur = *(slot as *const usize);
                    if cur == 0 {
                        break;
                    }
                    if cur == expected {
                        let mut old = 0u32;
                        let ok = VirtualProtect(slot as *const c_void, 8, PAGE_READWRITE, &mut old);
                        if ok == 0 {
                            return Err("IAT slot not writable".into());
                        }
                        *(slot as *mut usize) = hook;
                        let mut _tmp = 0u32;
                        VirtualProtect(slot as *const c_void, 8, old, &mut _tmp);
                        return Ok(slot - base);
                    }
                    slot += 8;
                }
                return Err(format!(
                    "{dll} imports found but target slot not among them"
                ));
            }
            d += 20;
        }
        Err(format!("{dll} not in host imports"))
    }
}

pub fn install(log: &mut Log) -> Result<String, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    unsafe {
        let vulkan = GetModuleHandleA(b"vulkan-1.dll\0".as_ptr());
        if vulkan.is_null() {
            return Err("vulkan-1.dll not loaded".into());
        }
        let expected = GetProcAddress(vulkan, b"vkQueuePresentKHR\0".as_ptr());
        let Some(expected_fn) = expected else {
            return Err("vkQueuePresentKHR not exported".into());
        };
        let expected_addr = expected_fn as usize;
        let slot_rva = swap_iat(
            "vulkan-1.dll",
            expected_addr,
            present_hook as *const () as usize,
        )?;
        PRESENT_ORIG = expected_addr;

        // Pump fallback: independent of the present result.
        let user32 = GetModuleHandleA(b"user32.dll\0".as_ptr());
        if !user32.is_null() {
            if let Some(peek_fn) = GetProcAddress(user32, b"PeekMessageW\0".as_ptr()) {
                let peek_addr = peek_fn as usize;
                match swap_iat("user32.dll", peek_addr, peek_hook as *const () as usize) {
                    Ok(rva) => {
                        PUMP_ORIG = peek_addr;
                        log.line(&format!("pump hook @ iat rva {rva:#x}"));
                    }
                    Err(e) => log.line(&format!("pump hook NOT INSTALLED - {e}")),
                }
            }
        }

        ensure_thread();
        log.line("overlay key is the grave key (left of 1; backtick on US layouts)");
        Ok(format!("present hook @ iat rva {slot_rva:#x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOD_TOML: &str =
        "[mod]\nid = \"x\"\nversion = \"0.0.0\"\nenabled = true\nlabel = \"X\"\n";

    #[test]
    fn enabled_rewrites_in_place() {
        let out = rewrite_enabled_in_text(MOD_TOML, false).unwrap();
        assert!(out.contains("enabled = false"));
        assert!(!out.contains("enabled = true"));
        // Every other line survives byte-for-byte.
        assert!(out.contains("id = \"x\"\nversion = \"0.0.0\"\n"));
        assert!(out.contains("label = \"X\""));
    }

    #[test]
    fn enabled_re_enables() {
        let off = rewrite_enabled_in_text(MOD_TOML, false).unwrap();
        let on = rewrite_enabled_in_text(&off, true).unwrap();
        assert!(out_lines_contain(&on, "enabled = true"));
        assert_eq!(on.lines().count(), MOD_TOML.lines().count());
    }

    #[test]
    fn enabled_inserts_when_absent() {
        // Mid-file: before the next section.
        let toml = "[mod]\nid = \"x\"\n\n[[setting]]\nid = \"s\"\nkind = \"bool\"\nlabel = \"S\"\ndefault = true\n";
        let out = rewrite_enabled_in_text(toml, false).unwrap();
        let mod_end = out.find("[[setting]]").unwrap();
        assert!(out[..mod_end].contains("enabled = false"));
        // Trailing [mod] section: appended at the end.
        let toml = "[[patch]]\nid = \"p\"\n\n[mod]\nid = \"x\"\n";
        let out = rewrite_enabled_in_text(toml, true).unwrap();
        assert!(out.ends_with("enabled = true"));
    }

    #[test]
    fn enabled_needs_mod_section() {
        assert!(rewrite_enabled_in_text("[[setting]]\nid = \"s\"\n", true).is_none());
        assert!(rewrite_enabled_in_text("", false).is_none());
    }

    #[test]
    fn enabled_ignores_same_named_setting_keys() {
        // A setting whose id is "enabled" must not be touched.
        let toml = "[mod]\nid = \"x\"\nenabled = true\n\n[[setting]]\nid = \"enabled\"\nkind = \"bool\"\nlabel = \"E\"\ndefault = true\n";
        let out = rewrite_enabled_in_text(toml, false).unwrap();
        assert_eq!(out.matches("enabled = false").count(), 1);
        assert!(out.contains("id = \"enabled\""));
    }

    #[test]
    fn grave_scancode_is_the_toggle() {
        assert!(is_toggle_scancode(SC_GRAVE));
        assert!(!is_toggle_scancode(0));
        assert!(!is_toggle_scancode(0x28));
        assert!(!is_toggle_scancode(VK_OEM_3 as u32));
    }

    #[test]
    fn toggle_gate_suppresses_on_steam_or_unfocused() {
        assert!(toggle_allowed(false, true));
        assert!(!toggle_allowed(true, true));
        assert!(!toggle_allowed(false, false));
        assert!(!toggle_allowed(true, false));
    }

    #[test]
    fn overlay_callback_tracks_active_byte() {
        let mut active = 1u8;
        unsafe { steam_overlay_run(std::ptr::null_mut(), &mut active as *mut u8 as *mut c_void) };
        assert!(steam_overlay_open());
        let mut inactive = 0u8;
        unsafe {
            steam_overlay_run(
                std::ptr::null_mut(),
                &mut inactive as *mut u8 as *mut c_void,
            )
        };
        assert!(!steam_overlay_open());
    }

    fn out_lines_contain(out: &str, line: &str) -> bool {
        out.lines().any(|l| l.trim() == line)
    }
}
