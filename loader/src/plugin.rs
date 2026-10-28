//! Native plugin loading.
//!
//! Two ways to get a DLL loaded:
//!
//! 1. Name it in a mod's `mod.toml` and ship it in that mod's folder.
//!    It follows the mod's `enabled` setting like everything else:
//!
//!    ```toml
//!    [mod]
//!    id = "eu.example.my_mod"
//!    version = "1.0.0"
//!    plugin = "MyMod.dll"
//!    ```
//!
//! 2. Drop it into a mod folder that has no mod.toml, like any other
//!    mod file: `GAMEDATA/MODS/MyMod/MyMod.dll`. Every .dll in such a
//!    folder is loaded.
//!
//! The framework only loads the DLL. The plugin starts itself from its
//! own DllMain, the same way it would as a standalone proxy.
//!
//! Loading happens on a short-lived thread instead of inside our
//! DllMain, because calling LoadLibrary while the loader lock is held
//! is how proxy DLLs deadlock. A new thread can't start running until
//! the loader lock is released, so by the time it calls LoadLibrary the
//! game has finished loading its imports (and our log is written).
//!
//! Results are added to the end of nozavolochen.log under "plugins".

use std::ffi::c_void;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

use crate::log::Log;

/// One plugin to load: who asked for it (a mod id, or the mod folder
/// it was dropped into) and the DLL's full path.
pub struct Plugin {
    pub owner: String,
    pub path: String,
}

/// A plugin must be a plain file name ending in `.dll`, so it can only
/// point at a file inside its own folder.
pub fn valid_name(name: &str) -> bool {
    name.len() > 4
        && name.to_ascii_lowercase().ends_with(".dll")
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
}

/// Hand the plugins to a loader thread. Returns false if the thread
/// could not be created (no plugin is loaded then).
pub fn start(hinst: *mut c_void, plugins: Vec<Plugin>) -> bool {
    if plugins.is_empty() {
        return true;
    }
    // The module handle is only used to find the log path; pass it as a
    // number so the closure can move to the new thread.
    let hinst = hinst as usize;
    std::thread::Builder::new()
        .name("nozavolochen-plugins".into())
        .spawn(move || unsafe { load_all(hinst as *mut c_void, &plugins) })
        .is_ok()
}

unsafe fn load_all(hinst: *mut c_void, plugins: &[Plugin]) {
    let mut log = Log::new("nozavolochen");
    log.section("plugins");
    for p in plugins {
        let wide: Vec<u16> = p.path.encode_utf16().chain(std::iter::once(0)).collect();
        if LoadLibraryW(wide.as_ptr()).is_null() {
            let err = GetLastError();
            log.line(&format!(
                "{}: NOT LOADED - {} (error {err})",
                p.owner, p.path
            ));
        } else {
            log.line(&format!("{}: loaded {}", p.owner, p.path));
        }
    }
    log.append_beside(hinst);
}
