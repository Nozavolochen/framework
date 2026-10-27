//! Tiny append-only log. Written once at the end of init.

use std::ffi::c_void;

pub struct Log {
    name: String,
    lines: Vec<String>,
}

/// Consolidated log directory: `<game>/Binaries/nozavolochen/`,
/// created on first use. Every host and mod log lives here so
/// Binaries itself stays clean.
pub fn dir() -> Option<String> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameA;
    unsafe {
        let mut buf = [0u8; MAX_PATH as usize];
        let n = GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), MAX_PATH as u32);
        if n == 0 {
            return None;
        }
        let mut path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        let sep = path.rfind(['/', '\\'])?;
        path.truncate(sep + 1);
        path.push_str("nozavolochen");
        let _ = std::fs::create_dir_all(&path);
        Some(path)
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with(['/', '\\']) {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Append one line to a log in the consolidated directory.
pub fn append(name: &str, line: &str) {
    if let Some(d) = dir() {
        let path = join(&d, name);
        let text = format!("{line}\n");
        if let Ok(existing) = std::fs::read(&path) {
            let mut prev = String::from_utf8_lossy(&existing).into_owned();
            prev.push_str(&text);
            let _ = std::fs::write(&path, prev);
        } else {
            let _ = std::fs::write(&path, text);
        }
    }
}

impl Log {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            lines: Vec::new(),
        }
    }

    pub fn header(&mut self, hinst: *mut c_void) {
        use windows_sys::Win32::Foundation::MAX_PATH;
        use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameA;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        self.lines.push(format!(
            "nozavolochen proxy xinput9_1_0.dll (module {:p})",
            hinst
        ));
        unsafe {
            let mut t = std::mem::zeroed();
            GetLocalTime(&mut t);
            // SYSTEMTIME layout: y,mo,dow,d,h,min,s,ms (8xu16)
            let w: &[u16; 8] = std::mem::transmute(&t);
            self.lines.push(format!(
                "time      : {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                w[0], w[1], w[3], w[4], w[5], w[6]
            ));
            let mut buf = [0u8; MAX_PATH as usize];
            let n = GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), MAX_PATH as u32);
            if n > 0 {
                let host = String::from_utf8_lossy(&buf[..n as usize]);
                self.lines.push(format!("host      : {host}"));
            }
        }
        self.lines.push(String::from("patches"));
    }

    pub fn line(&mut self, s: &str) {
        self.lines.push(format!("  {s}"));
    }

    /// Write nozavolochen.log into the consolidated log directory.
    pub fn write_beside(&self, _hinst: *mut c_void) {
        if let Some(d) = dir() {
            let path = join(&d, "nozavolochen.log");
            let text = self.lines.join("\r\n") + "\r\n";
            let _ = std::fs::write(&path, text);
        }
        let _ = &self.name;
    }
}
