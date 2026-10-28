//! Nozavolochen: single xinput9_1_0.dll proxy hosting independent
//! declarative runtime mods for No Man's Sky.
//!
//! Entry runs synchronously on attach: forwarders first (the game needs
//! them immediately), then mod discovery, then spec ops, then the log.
//! Everything is fail-loud per patch; one broken signature never blocks
//! the others or the controller forwarding.

use std::ffi::c_void;

mod host;
mod image;
mod locale;
mod log;
mod ops;
mod overlay;
mod patch;
mod proxy;
mod spec;

const DLL_PROCESS_ATTACH: u32 = 1;

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hinst: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    // No thread notifications; all work is done inline on attach.
    proxy::disable_thread_calls(hinst);
    init(hinst);
    1
}

unsafe fn init(hinst: *mut c_void) {
    let mut log = log::Log::new("nozavolochen");
    log.header(hinst);

    if !proxy::host_is_nms() {
        log.line("host is not NMS.exe: forwarding xinput only, mods skipped");
        proxy::load_real(&mut log);
        log.write_beside(hinst);
        return;
    }

    proxy::load_real(&mut log);
    log.line(&format!(
        "loader: {} (build {})",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_ID")
    ));

    match overlay::install(&mut log) {
        Ok(detail) => log.line(&format!("overlay: {detail}")),
        Err(e) => log.line(&format!("overlay: NOT INSTALLED - {e}")),
    }

    let snapshots = host::snapshot(&mut log);
    let mods: Vec<crate::spec::Mod> = snapshots.into_iter().map(|s| s.spec).collect();
    let summary = mods
        .iter()
        .map(|p| format!("{}@{}", p.id, p.version))
        .collect::<Vec<_>>()
        .join(" ");
    log.line(&format!("mods: {summary}"));
    let (lang, source) = locale::current();
    log.line(&format!("language: {lang} ({source})"));

    let mut done = 0;
    let mut total = 0;
    for p in &mods {
        if !p.enabled {
            log.line(&format!("{}: disabled in config", p.id));
            continue;
        }
        for patch in &p.patches {
            if !ops::patch_on(p, patch) {
                log.line(&format!("{}: disabled in config", patch.id));
                continue;
            }
            total += 1;
            match ops::run(patch, &p.settings, &mut log) {
                Ok(detail) => {
                    done += 1;
                    log.line(&format!("{}: applied {detail}", patch.id));
                }
                Err(e) => log.line(&format!("{}: NOT APPLIED - {e}", patch.id)),
            }
        }
    }

    log.line(&format!("result: {done}/{total} applied"));
    log.write_beside(hinst);
}
