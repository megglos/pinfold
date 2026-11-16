//! Plain text log next to the config. Written only on events.
//! Size-capped at 1 MB with one rotation (pinfold.log -> pinfold.log.1).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 1024 * 1024;

static ECHO: AtomicBool = AtomicBool::new(false);

/// Also print every entry to stdout (console `run` mode).
pub fn set_echo(on: bool) {
    ECHO.store(on, Ordering::Relaxed);
}

fn timestamp() -> String {
    unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        )
    }
}

pub fn log(msg: &str) {
    let line = format!("{} {}", timestamp(), msg);
    if ECHO.load(Ordering::Relaxed) {
        crate::win::say(&line);
    }
    let dir = crate::config::dir();
    let path = crate::config::log_path(&dir);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let mut old = path.clone().into_os_string();
            old.push(".1");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
    } else {
        let _ = std::fs::create_dir_all(&dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}
