//! Tiny append-only log. Written once at the end of init.

use std::ffi::c_void;

pub struct Log {
    name: String,
    lines: Vec<String>,
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

    /// Start a new heading, like "patches" in the header.
    pub fn section(&mut self, name: &str) {
        self.lines.push(name.into());
    }

    /// Write nozavolochen.log next to our DLL.
    pub fn write_beside(&self, hinst: *mut c_void) {
        self.save(hinst, false);
    }

    /// Add to the end of nozavolochen.log. Used by the plugin thread,
    /// which runs after init has already written the log.
    pub fn append_beside(&self, hinst: *mut c_void) {
        self.save(hinst, true);
    }

    fn save(&self, hinst: *mut c_void, append: bool) {
        use std::io::Write;
        use windows_sys::Win32::Foundation::MAX_PATH;
        use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameA;
        unsafe {
            let mut buf = [0u8; MAX_PATH as usize];
            let n = GetModuleFileNameA(hinst, buf.as_mut_ptr(), MAX_PATH as u32);
            if n == 0 {
                return;
            }
            let mut path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
            if let Some(sep) = path.rfind(['/', '\\']) {
                path.truncate(sep + 1);
            } else {
                path.clear();
            }
            path.push_str("nozavolochen.log");
            let text = self.lines.join("\r\n") + "\r\n";
            if append {
                let file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path);
                if let Ok(mut f) = file {
                    let _ = f.write_all(text.as_bytes());
                }
            } else {
                let _ = std::fs::write(&path, text);
            }
        }
        let _ = &self.name;
    }
}
