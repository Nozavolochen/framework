//! Mod discovery and assembly.
//!
//! Every `GAMEDATA/MODS/*/mod.toml` is loaded by `[mod] id`. The
//! framework ships no mods itself: with no mod folders present,
//! nothing is patched. A mod folder with an unparsable spec is
//! reported and skipped; it never blocks others.

use crate::log::Log;
use crate::spec::{parse_mod, Mod};

use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// `<game>` (the folder holding `Binaries`), derived from the host exe.
pub(crate) fn game_root() -> Option<std::path::PathBuf> {
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
        Some(std::path::PathBuf::from(&host[..game_end]))
    }
}

/// `<game>/GAMEDATA/MODS`, derived from the host exe in `<game>/Binaries`.
pub(crate) fn mods_dir() -> Option<String> {
    let game = game_root()?.into_os_string().into_string().ok()?;
    let sep = if game.contains('\\') { "\\" } else { "/" };
    Some(format!("{game}{sep}GAMEDATA{sep}MODS"))
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

/// One loaded mod folder with the mod.toml mtime it was read at.
pub struct Snapshot {
    folder: String,
    mtime: Option<SystemTime>,
    pub spec: Mod,
}

/// One mod folder whose mod.toml failed to parse: kept so the
/// overlay Mods page can list it with its error instead of
/// silently dropping it (the log line alone is invisible in game).
#[derive(Debug, Clone)]
pub struct LoadError {
    pub folder: String,
    pub error: String,
}

static KNOWN: OnceLock<Mutex<Vec<Snapshot>>> = OnceLock::new();
static KNOWN_ERRORS: OnceLock<Mutex<Vec<LoadError>>> = OnceLock::new();

fn mod_path(folder: &str) -> Option<String> {
    let dir = mods_dir()?;
    let sep = if dir.contains('\\') { "\\" } else { "/" };
    Some(format!("{dir}{sep}{folder}{sep}mod.toml"))
}

fn file_mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Load every mod folder (as discover did) and remember each
/// folder's mod.toml mtime for live reload.
pub fn snapshot(log: &mut Log) -> Vec<Snapshot> {
    let mut out: Vec<Snapshot> = Vec::new();
    let mut errors: Vec<LoadError> = Vec::new();
    let Some(dir) = mods_dir() else {
        log.line("mods: host path unknown, no mods loaded");
        return out;
    };
    for sub in subdirs(&dir) {
        let path = match mod_path(&sub) {
            Some(p) => p,
            None => continue,
        };
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
                let snap = Snapshot {
                    folder: sub,
                    mtime: file_mtime(&path),
                    spec: p,
                };
                if let Some(slot) = out.iter_mut().find(|e| e.spec.id == snap.spec.id) {
                    *slot = snap;
                } else {
                    out.push(snap);
                }
            }
            Err(e) => {
                log.line(&format!("MODS\\{sub}: mod.toml NOT LOADED - {e}"));
                if let Some(slot) = errors.iter_mut().find(|x| x.folder == sub) {
                    slot.error = e;
                } else {
                    errors.push(LoadError {
                        folder: sub,
                        error: e,
                    });
                }
            }
        }
    }
    let _ = KNOWN.set(Mutex::new(
        out.iter()
            .map(|s| Snapshot {
                folder: s.folder.clone(),
                mtime: s.mtime,
                spec: s.spec.clone(),
            })
            .collect(),
    ));
    let _ = KNOWN_ERRORS.set(Mutex::new(errors));
    out
}

/// (mod id, folder) for every loaded mod, for write-back.
pub fn folder_map() -> Vec<(String, String)> {
    let Some(known) = KNOWN.get() else {
        return Vec::new();
    };
    let Ok(guard) = known.lock() else {
        return Vec::new();
    };
    guard
        .iter()
        .map(|s| (s.spec.id.clone(), s.folder.clone()))
        .collect()
}

/// (folder, error) for every mod folder that failed to parse, for
/// the overlay Mods page. Folders that parsed never appear here.
pub fn load_errors() -> Vec<(String, String)> {
    let Some(known) = KNOWN_ERRORS.get() else {
        return Vec::new();
    };
    let Ok(guard) = known.lock() else {
        return Vec::new();
    };
    guard
        .iter()
        .map(|e| (e.folder.clone(), e.error.clone()))
        .collect()
}

/// The currently loaded mod specs, for overlay row building.
pub fn current_mods() -> Vec<Mod> {
    let Some(known) = KNOWN.get() else {
        return Vec::new();
    };
    let Ok(guard) = known.lock() else {
        return Vec::new();
    };
    guard.iter().map(|s| s.spec.clone()).collect()
}

/// Re-read any mod.toml whose mtime moved and push changed tuning
/// into the applied patches. Patterns never re-resolve live: only
/// tuning keys travel this path, and a failed re-parse keeps the old
/// tuning. Called from the present hook every couple of seconds.
/// Returns true when any snapshot changed (callers rebuild UI rows).
pub fn poll_reload() -> bool {
    poll_impl(true)
}

/// Same as poll_reload without success lines: the overlay calls this
/// right after writing a value, so per-step successes stay quiet
/// while failures still log.
pub fn poll_reload_quiet() -> bool {
    poll_impl(false)
}

fn poll_impl(loud: bool) -> bool {
    let Some(known) = KNOWN.get() else {
        return false;
    };
    let Ok(mut guard) = known.lock() else {
        return false;
    };
    let mut changed = false;
    for snap in guard.iter_mut() {
        let Some(path) = mod_path(&snap.folder) else {
            continue;
        };
        let mtime = file_mtime(&path);
        if mtime == snap.mtime {
            continue;
        }
        // Record the new mtime even when the re-parse fails, so one
        // bad edit logs once instead of spamming every poll.
        snap.mtime = mtime;
        changed = true;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match parse_mod(&text) {
            Ok(spec) => {
                for patch in &spec.patches {
                    let enabled = crate::ops::patch_on(&spec, patch);
                    let outcome = crate::ops::retune(patch, &spec.settings, enabled);
                    // Overlay-driven reloads stay quiet on success (the
                    // panel already shows the value); failures always log.
                    if loud || outcome.is_err() {
                        let line = match outcome {
                            Ok(detail) => {
                                format!("  MODS\\{}: {} reloaded - {detail}", snap.folder, patch.id)
                            }
                            Err(e) => {
                                format!("  MODS\\{}: {} NOT UPDATED - {e}", snap.folder, patch.id)
                            }
                        };
                        crate::log::append("nozavolochen.log", &line);
                    }
                }
                // A fixed file stops being a load error.
                if let Some(errs) = KNOWN_ERRORS.get() {
                    if let Ok(mut guard) = errs.lock() {
                        guard.retain(|x| x.folder != snap.folder);
                    }
                }
                snap.spec = spec;
            }
            Err(e) => {
                crate::log::append(
                    "nozavolochen.log",
                    &format!("  MODS\\{}: mod.toml NOT RELOADED - {e}", snap.folder),
                );
                // A newly broken file joins the overlay Mods page errors.
                if let Some(errs) = KNOWN_ERRORS.get() {
                    if let Ok(mut guard) = errs.lock() {
                        if let Some(slot) = guard.iter_mut().find(|x| x.folder == snap.folder) {
                            slot.error.clone_from(&e);
                        } else {
                            guard.push(LoadError {
                                folder: snap.folder.clone(),
                                error: e,
                            });
                        }
                    }
                }
            }
        }
    }
    changed
}
