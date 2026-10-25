//! Pattern scanning + patch writing against the host NMS.exe.
//!
//! The scan covers the host's .text section (located via its own PE
//! headers at runtime, so ASLR/base changes need no handling). Every
//! patch verifies the exact bytes it expects before writing and reports
//! the RVA applied at, so logs stay comparable across game versions.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleA;
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Base address + length of the host image's .text section.
pub fn host_text() -> Result<(usize, usize), String> {
    unsafe {
        let base = GetModuleHandleA(std::ptr::null());
        if base.is_null() {
            return Err("GetModuleHandleA(NULL) failed".into());
        }
        let b = base as *const u8;
        if *(b as *const u16) != 0x5A4D {
            return Err("host missing MZ header".into());
        }
        let lfanew = *(b.add(0x3C) as *const u32) as usize;
        let nsec = *(b.add(lfanew + 6) as *const u16) as usize;
        // Optional header magic must be PE32+ (x86-64).
        let magic = *(b.add(lfanew + 24) as *const u16);
        if magic != 0x20B {
            return Err("host is not a 64-bit image".into());
        }
        let sections = lfanew + 24 + 240;
        for i in 0..nsec {
            let s = b.add(sections + i * 40);
            if &*s.add(0) == &b'.' && *(s.add(1)) == b't' {
                // ".text" name check on the first 5 bytes.
                let name = std::slice::from_raw_parts(s, 5);
                if name == b".text" {
                    let vaddr = *(s.add(12) as *const u32) as usize;
                    let vsize = *(s.add(8) as *const u32) as usize;
                    return Ok((base as usize + vaddr, vsize));
                }
            }
        }
        Err("host .text section not found".into())
    }
}

/// Naive masked scan. mask[i] != 0 means the byte must match.
/// Returns RVAs-as-absolute-addresses of every hit.
pub fn scan(base: usize, len: usize, pat: &[u8], mask: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if pat.len() != mask.len() || pat.is_empty() || len < pat.len() {
        return out;
    }
    unsafe {
        let mem = std::slice::from_raw_parts(base as *const u8, len);
        let n = pat.len();
        let mut i = 0;
        while i + n <= len {
            let mut ok = true;
            for j in 0..n {
                if mask[j] != 0 && mem[i + j] != pat[j] {
                    ok = false;
                    break;
                }
            }
            if ok {
                out.push(base + i);
            }
            i += 1;
        }
    }
    out
}

pub fn read_bytes(addr: usize, n: usize) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(addr as *const u8, n).to_vec() }
}

pub fn read_i32_le(addr: usize) -> i32 {
    unsafe { *(addr as *const i32) }
}

/// Write bytes with PAGE_EXECUTE_READWRITE + icache flush.
/// Returns Err without writing anything if `expect` is Some and mismatched.
pub fn write_bytes(addr: usize, data: &[u8], expect: Option<&[u8]>) -> Result<(), String> {
    if data.is_empty() {
        return Err("empty patch".into());
    }
    unsafe {
        if let Some(e) = expect {
            let cur = std::slice::from_raw_parts(addr as *const u8, e.len());
            if cur != e {
                return Err(format!(
                    "precondition bytes changed (wanted {:02x?}, found {:02x?})",
                    e, cur
                ));
            }
        }
        let mut old = 0u32;
        let ok = VirtualProtect(
            addr as *const c_void_poly,
            data.len(),
            PAGE_EXECUTE_READWRITE,
            &mut old,
        );
        if ok == 0 {
            return Err("VirtualProtect failed".into());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
        let mut _tmp = 0u32;
        VirtualProtect(addr as *const c_void_poly, data.len(), old, &mut _tmp);
        FlushInstructionCache(GetCurrentProcess(), addr as *const c_void_poly, data.len());
    }
    Ok(())
}

// windows-sys expects *const c_void; keep one alias to avoid repetition.
#[allow(non_camel_case_types)]
type c_void_poly = std::ffi::c_void;

/// Format an absolute address as an RVA against the host image base.
pub fn rva_of(addr: usize) -> String {
    unsafe {
        let base = GetModuleHandleA(std::ptr::null()) as usize;
        format!("{:#x}", addr.wrapping_sub(base))
    }
}
