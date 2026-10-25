//! Minimal xinput9_1_0.dll proxy.
//!
//! The game imports seven XInput functions by name. We resolve each from
//! the real system DLL (loaded from the System32 directory, never from the
//! game folder which would recurse into ourselves) and forward calls.
//! Struct layouts are deliberately opaque (*mut c_void): we only pass
//! pointers through, so no layout knowledge is needed.

use std::ffi::{c_void, CString};

use windows_sys::Win32::Foundation::MAX_PATH;
use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameA, GetProcAddress, LoadLibraryA,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryA;

type FnEnable = unsafe extern "system" fn(u32);
type Fn2 = unsafe extern "system" fn(u32, *mut c_void) -> u32;
type Fn3 = unsafe extern "system" fn(u32, u32, *mut c_void) -> u32;
type FnGuids = unsafe extern "system" fn(u32, *mut c_void, *mut c_void) -> u32;

static mut REAL_ENABLE: Option<FnEnable> = None;
static mut REAL_BATTERY: Option<Fn3> = None;
static mut REAL_CAPS: Option<Fn3> = None;
static mut REAL_GUIDS: Option<FnGuids> = None;
static mut REAL_KEYSTROKE: Option<Fn3> = None;
static mut REAL_GETSTATE: Option<Fn2> = None;
static mut REAL_SETSTATE: Option<Fn2> = None;

macro_rules! fwd {
    ($static:ident, $ty:ty, $name:expr) => {{
        match unsafe { $static } {
            Some(f) => f,
            None => {
                return missing($name);
            }
        }
    }};
}

fn missing(_name: &str) -> u32 {
    // ERROR_DEVICE_NOT_CONNECTED: the game treats the controller as absent.
    1167
}

#[no_mangle]
pub unsafe extern "system" fn XInputEnable(enable: u32) {
    if let Some(f) = REAL_ENABLE {
        f(enable);
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetBatteryInformation(
    user: u32,
    dev: u32,
    info: *mut c_void,
) -> u32 {
    fwd!(REAL_BATTERY, Fn3, "battery")(user, dev, info)
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetCapabilities(
    user: u32,
    flags: u32,
    caps: *mut c_void,
) -> u32 {
    fwd!(REAL_CAPS, Fn3, "caps")(user, flags, caps)
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetDSoundAudioDeviceGuids(
    user: u32,
    render: *mut c_void,
    capture: *mut c_void,
) -> u32 {
    fwd!(REAL_GUIDS, FnGuids, "guids")(user, render, capture)
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetKeystroke(
    user: u32,
    reserved: u32,
    key: *mut c_void,
) -> u32 {
    fwd!(REAL_KEYSTROKE, Fn3, "keystroke")(user, reserved, key)
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetState(user: u32, state: *mut c_void) -> u32 {
    fwd!(REAL_GETSTATE, Fn2, "getstate")(user, state)
}

#[no_mangle]
pub unsafe extern "system" fn XInputSetState(user: u32, vib: *mut c_void) -> u32 {
    fwd!(REAL_SETSTATE, Fn2, "setstate")(user, vib)
}

pub fn disable_thread_calls(hinst: *mut c_void) {
    unsafe {
        DisableThreadLibraryCalls(hinst as *mut c_void);
    }
}

/// True when the host process image is NMS.exe (case-insensitive).
/// Anything else (e.g. a tool probing the DLL) gets forwarding only.
pub fn host_is_nms() -> bool {
    unsafe {
        let mut buf = [0u8; MAX_PATH as usize];
        let n = GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), MAX_PATH as u32);
        if n == 0 || n as usize >= buf.len() {
            return false;
        }
        let path = String::from_utf8_lossy(&buf[..n as usize]);
        path.rsplit(['/', '\\'])
            .next()
            .map(|f| f.eq_ignore_ascii_case("NMS.exe"))
            .unwrap_or(false)
    }
}

fn load_addr(lib: *mut c_void, export: &str) -> Option<*mut c_void> {
    let name = CString::new(export).ok()?;
    let p = unsafe { GetProcAddress(lib as *mut _, name.as_ptr() as *const u8) };
    p.map(|f| f as *mut c_void)
}

/// Load the real xinput9_1_0.dll from the system directory and resolve
/// all seven forwards. Missing functions stay None (callers get 1167).
pub fn load_real(log: &mut crate::log::Log) {
    unsafe {
        let mut dir = [0u8; MAX_PATH as usize];
        let n = GetSystemDirectoryA(dir.as_mut_ptr(), MAX_PATH as u32);
        if n == 0 {
            log.line("forwarder: GetSystemDirectoryA failed");
            return;
        }
        let mut path = String::from_utf8_lossy(&dir[..n as usize]).into_owned();
        path.push_str("\\xinput9_1_0.dll");
        let cpath = match CString::new(path) {
            Ok(p) => p,
            Err(_) => return,
        };
        let lib = LoadLibraryA(cpath.as_ptr() as *const u8);
        if lib.is_null() {
            log.line("forwarder: system xinput9_1_0.dll not found");
            return;
        }
        // Transmute through fn() then to each concrete type; all are
        // system-ABI function pointers of pointer width.
        if let Some(p) = load_addr(lib, "XInputEnable") {
            REAL_ENABLE = Some(std::mem::transmute::<*mut c_void, FnEnable>(p));
        }
        if let Some(p) = load_addr(lib, "XInputGetBatteryInformation") {
            REAL_BATTERY = Some(std::mem::transmute::<*mut c_void, Fn3>(p));
        }
        if let Some(p) = load_addr(lib, "XInputGetCapabilities") {
            REAL_CAPS = Some(std::mem::transmute::<*mut c_void, Fn3>(p));
        }
        if let Some(p) = load_addr(lib, "XInputGetDSoundAudioDeviceGuids") {
            REAL_GUIDS = Some(std::mem::transmute::<*mut c_void, FnGuids>(p));
        }
        if let Some(p) = load_addr(lib, "XInputGetKeystroke") {
            REAL_KEYSTROKE = Some(std::mem::transmute::<*mut c_void, Fn3>(p));
        }
        if let Some(p) = load_addr(lib, "XInputGetState") {
            REAL_GETSTATE = Some(std::mem::transmute::<*mut c_void, Fn2>(p));
        }
        if let Some(p) = load_addr(lib, "XInputSetState") {
            REAL_SETSTATE = Some(std::mem::transmute::<*mut c_void, Fn2>(p));
        }
    }
}
