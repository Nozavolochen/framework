//! Mod discovery and assembly.
//!
//! Every `GAMEDATA/MODS/*/mod.toml` is loaded by `[mod] id`. The
//! framework ships no mods itself: with no mod folders present,
//! nothing is patched. A mod folder with an unparsable spec is
//! reported and skipped; it never blocks others.

use crate::log::Log;
use crate::spec::{parse_mod, Mod};

/// `<game>/GAMEDATA/MODS`, derived from the host exe in `<game>/Binaries`.
fn mods_dir() -> Option<String> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameA;
    unsafe {
        let mut buf = [0u8; MAX_PATH as usize];
        let n = GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), MAX_PATH as u32);
        if n == 0 {
            return None;
        }
        let host = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        let bin_end = host.rfind(['/', '\\'])?;
        let game_end = host[..bin_end].rfind(['/', '\\'])?;
        let game = &host[..game_end];
        let sep = if host.contains('\\') { "\\" } else { "/" };
        Some(format!("{game}{sep}GAMEDATA{sep}MODS"))
    }
}

/// Names of subdirectories directly under `dir`.
fn subdirs(dir: &str) -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileA, FindNextFileA, FILE_ATTRIBUTE_DIRECTORY, WIN32_FIND_DATAA,
    };
    let mut out = Vec::new();
    unsafe {
        let pat = format!("{dir}\\*");
        let mut data: WIN32_FIND_DATAA = std::mem::zeroed();
        let h = FindFirstFileA(format!("{pat}\0").as_ptr(), &mut data);
        if h.is_null() || h as isize == -1 {
            return out;
        }
        loop {
            let attrs = data.dwFileAttributes;
            let raw = &data.cFileName;
            let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let bytes: &[u8] = std::slice::from_raw_parts(raw.as_ptr() as *const u8, len);
            let name = String::from_utf8_lossy(bytes).into_owned();
            if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 && name != "." && name != ".." {
                out.push(name);
            }
            if FindNextFileA(h, &mut data) == 0 {
                break;
            }
        }
        FindClose(h);
    }
    out
}

pub fn discover(log: &mut Log) -> Vec<Mod> {
    let mut mods: Vec<Mod> = Vec::new();
    let Some(dir) = mods_dir() else {
        log.line("mods: host path unknown, no mods loaded");
        return mods;
    };
    for sub in subdirs(&dir) {
        let path = format!("{dir}\\{sub}\\mod.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match parse_mod(&text) {
            Ok(mut p) => {
                p.dir = sub.clone();
                log.line(&format!(
                    "mod {} {}: spec loaded from MODS\\{sub}",
                    p.id, p.version
                ));
                if let Some(slot) = mods.iter_mut().find(|e| e.id == p.id) {
                    *slot = p;
                } else {
                    mods.push(p);
                }
            }
            Err(e) => log.line(&format!("MODS\\{sub}: mod.toml NOT LOADED - {e}")),
        }
    }
    mods
}

/// DLLs dropped into mod folders that have no mod.toml, as
/// (folder, full path), sorted. Mods with a mod.toml name their plugin
/// instead, so a stray DLL there is never picked up.
pub fn loose_plugins() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let Some(dir) = mods_dir() else {
        return found;
    };
    for sub in subdirs(&dir) {
        let folder = format!("{dir}\\{sub}");
        if std::path::Path::new(&format!("{folder}\\mod.toml")).exists() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_file()) && crate::plugin::valid_name(&name) {
                found.push((sub.clone(), format!("{folder}\\{name}")));
            }
        }
    }
    found.sort();
    found
}

/// Full path of a mod's plugin DLL, or why it can't be used.
pub fn plugin_path(m: &Mod, name: &str) -> Result<String, String> {
    if !crate::plugin::valid_name(name) {
        return Err(format!(
            "{name:?} must be a .dll file name in the mod folder"
        ));
    }
    let dir = mods_dir().ok_or("host path unknown")?;
    let path = format!("{dir}\\{}\\{name}", m.dir);
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("{name} not found in MODS\\{}", m.dir));
    }
    Ok(path)
}
