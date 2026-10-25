//! Mod discovery and assembly.
//!
//! Order: embedded specs first, then every `GAMEDATA/MODS/*/mod.toml`
//! overrides by `[mod] id` (external wins, logged). A mod folder with
//! an unparsable spec is reported and skipped; it never blocks others.

use crate::log::Log;
use crate::spec::{parse_mod, Mod};

const EMBEDDED: &[(&str, &str)] = &[
    (
        "eu.savagecore.intro_skip",
        include_str!("../mods/intro_skip.toml"),
    ),
    (
        "eu.savagecore.mod_warning_skip",
        include_str!("../mods/mod_warning_skip.toml"),
    ),
    (
        "eu.savagecore.bigger_item_grids_fixes",
        include_str!("../mods/bigger_item_grids_fixes.toml"),
    ),
];

fn embedded() -> Vec<Mod> {
    let mut out = Vec::new();
    for (_name, text) in EMBEDDED {
        // Embedded specs are reviewed in-tree; a parse failure here is a
        // build-time bug. Fail loud at runtime rather than mis-patching.
        match parse_mod(text) {
            Ok(p) => out.push(p),
            Err(e) => out.push(Mod {
                id: format!("BROKEN-EMBEDDED-SPEC ({e})"),
                version: String::new(),
                enabled: false,
                features: Vec::new(),
                patches: Vec::new(),
            }),
        }
    }
    out
}

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
    let mut mods = embedded();
    let Some(dir) = mods_dir() else {
        log.line("mods: host path unknown, embedded specs only");
        return mods;
    };
    for sub in subdirs(&dir) {
        let path = format!("{dir}\\{sub}\\mod.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match parse_mod(&text) {
            Ok(p) => {
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
