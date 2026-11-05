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
//! Results are appended to nozavolochen.log under "plugins".

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

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
pub fn start(plugins: Vec<Plugin>) -> bool {
    if plugins.is_empty() {
        return true;
    }
    std::thread::Builder::new()
        .name("nozavolochen-plugins".into())
        .spawn(move || load_all(&plugins))
        .is_ok()
}

fn load_all(plugins: &[Plugin]) {
    crate::log::append("nozavolochen.log", "plugins");
    for p in plugins {
        let wide: Vec<u16> = p.path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if LoadLibraryW(wide.as_ptr()).is_null() {
                let err = GetLastError();
                crate::log::append(
                    "nozavolochen.log",
                    &format!("  {}: NOT LOADED - {} (error {err})", p.owner, p.path),
                );
            } else {
                crate::log::append(
                    "nozavolochen.log",
                    &format!("  {}: loaded {}", p.owner, p.path),
                );
            }
        }
    }
}
