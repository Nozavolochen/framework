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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::log::Log;
use crate::spec::{Mod, Setting, SettingKind};
use crate::strings::Strings;

/// Backtick: the overlay key.
const VK_OEM_3: i32 = 0xC0;
const VK_SHIFT: i32 = 0x10;

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

fn key_down(vk: i32) -> bool {
    let hooked = (vk as usize) < 256 && HOOK_DOWN[vk as usize].load(Ordering::Relaxed);
    hooked
        || unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) as u16 & 0x8000
                != 0
        }
}

/// Forget hook-seen keys (on close), so a missed key-up can never
/// stick a key down.
fn clear_hook_keys() {
    for slot in HOOK_DOWN.iter() {
        slot.store(false, Ordering::Relaxed);
    }
}

/// The foreground window, when it belongs to this process. Typing
/// backtick in another app while the game runs must not toggle us.
fn foreground_window() -> Option<isize> {
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
    collapsed: bool,
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
    groups: Vec<Group>,
    /// Flat rows and the selected index.
    rows: Vec<Row>,
    /// Selectable items (headers + visible rows).
    items: Vec<Item>,
    selected: usize,
    /// Key state for edge detection + repeat.
    prev: Vec<(i32, bool, u32)>,
    /// Text being typed (setting index, buffer).
    editing: Option<(usize, String)>,
    /// Optimistic values written this session, until the reload
    /// picks them up: ((mod, setting), value, tick).
    pending: Vec<((String, String), String, u64)>,
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
    /// collapsed groups by mod id.
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
            // The overlay is a settings UI: mods with nothing to
            // tune stay out (their patches still apply at boot).
            if m.settings.is_empty() {
                continue;
            }
            groups.push(Group {
                id: m.id.clone(),
                label: if m.label.is_empty() {
                    m.id.clone()
                } else {
                    m.label.clone()
                },
                description: m.description.clone(),
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
        self.dirty = true;
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
            self.rebuild(&crate::host::current_mods());
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

    /// Handle one frame of input. Returns false when the overlay
    /// should close. Keyboard only: the game holds the pointer grab
    /// under Proton, so there is no mouse to speak of.
    fn input(&mut self) -> bool {
        // Escape closes (backtick is handled by the toggle).
        if key_edge(self, 0x1B).0 {
            self.editing = None;
            return false;
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
        // Delete resets the selection to its default.
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
                        self.reset_row(&row);
                    }
                }
                Some(Item::Row(_)) => {
                    if let Some(row) = self.selected_row_item() {
                        self.reset_row(&row);
                    }
                }
                None => {}
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
            if wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN {
                if swallow_key(kb.vkCode) {
                    return 1;
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam)
    }
}

/// Whether the game should not see this key right now: overlay keys
/// while open (plus printable keys in text entry), and only when the
/// game itself is foreground so other apps are never affected.
fn swallow_key(vk: u32) -> bool {
    if !OPEN.load(Ordering::Relaxed) {
        return false;
    }
    if foreground_window().is_none() {
        return false;
    }
    match vk {
        // Navigation, toggle, reset, close.
        0x25..=0x28 | 0x0D | 0x1B | 0x2E | 0xC0 => true,
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
    let down = key_down(VK_OEM_3);
    unsafe {
        let was = WAS_DOWN;
        WAS_DOWN = down;
        if down && !was {
            if foreground_window().is_some() {
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
                    st.rebuild(&crate::host::current_mods());
                }
            }
        }
    }
}

/// One open frame: input, window sync, repaint when dirty.
fn frame() {
    let game = match foreground_window() {
        Some(h) => h,
        None => {
            // Game lost focus: close rather than linger.
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
        // Cheap (one small file read): the player may have changed the
        // language since the last open.
        let (lang, how) = match crate::host::game_root() {
            Some(root) => crate::strings::detect_in(&root),
            None => ("en", "fallback"),
        };
        crate::log::append(
            "nozavolochen.log",
            &format!("overlay: language {lang} ({how})"),
        );
        self.strings = Strings::load(lang, None);
        self.rebuild(&crate::host::current_mods());
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
        use windows_sys::Win32::Graphics::Gdi::{DrawTextA, GetTextExtentPoint32A, SetTextColor};
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
                let ktext = format!("{key}\0");
                DrawTextA(
                    hdc,
                    ktext.as_ptr(),
                    -1,
                    &mut krc,
                    0x00000000 | 0x00000020 | 0x00000800,
                );
                let mut ext = SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32A(hdc, ktext.as_ptr(), key.len() as i32, &mut ext);
                x += ext.cx;
                SetTextColor(hdc, 0x00909090);
                let mut arc = RECT {
                    left: x,
                    top: y,
                    right: w - 16,
                    bottom: y + 22,
                };
                let atext = format!("{action}\0");
                DrawTextA(
                    hdc,
                    atext.as_ptr(),
                    -1,
                    &mut arc,
                    0x00000000 | 0x00000020 | 0x00000800,
                );
                GetTextExtentPoint32A(hdc, atext.as_ptr(), action.len() as i32, &mut ext);
                x += ext.cx;
            }
        }
    }

    fn panel_h(&self) -> i32 {
        136 + self
            .items
            .iter()
            .map(|it| self.item_height(it))
            .sum::<i32>()
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
                CreateFontA(
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
                    b"Consolas\0".as_ptr(),
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
            DrawTextA(
                hdc,
                b"Nozavolochen\0".as_ptr(),
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
            SelectObject(hdc, regular);
            // Groups and rows.
            let mut y = 52;
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
                        let text = format!(
                            "{}{} {}\0",
                            if n == self.selected { "> " } else { "  " },
                            marker,
                            group.label
                        );
                        DrawTextA(
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
                            let dtext = format!("{}\0", group.description);
                            DrawTextA(
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
                        // Pending (unconfirmed) values render amber; the rest
                        // follow the selection.
                        let pending = self.is_pending(*r);
                        SetTextColor(
                            hdc,
                            if pending {
                                0x0082CDFF
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
                        let label_text = format!("{}{}:\0", marker, row.setting.label);
                        DrawTextA(
                            hdc,
                            label_text.as_ptr(),
                            -1,
                            &mut rrc,
                            0x00000000 | 0x00000020 | 0x00000800,
                        );
                        // Value right-aligned.
                        let mut ext = SIZE { cx: 0, cy: 0 };
                        GetTextExtentPoint32A(hdc, val.as_ptr(), val.len() as i32, &mut ext);
                        let mut vrc = RECT {
                            left: w - 20 - ext.cx,
                            top: y,
                            right: w - 20,
                            bottom: y + 26,
                        };
                        let vtext = format!("{val}\0");
                        DrawTextA(
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
                            let dtext = format!("{}\0", row.setting.description);
                            DrawTextA(
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
                    ("esc", &format!(" {}  ", s.get("chrome.close"))),
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
        log.line("overlay key is backtick (`)");
        Ok(format!("present hook @ iat rva {slot_rva:#x}"))
    }
}
