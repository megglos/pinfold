//! One background instance for each config directory, and how to stop it.

use super::*;

/// Identifies instances with different config directories (tests). Empty
/// for the normal instance.
pub(super) fn instance_key() -> String {
    match std::env::var("PINFOLD_DIR") {
        Ok(dir) => {
            let mut hash = 0xcbf2_9ce4_8422_2325u64; // FNV-1a
            for b in dir.to_lowercase().bytes() {
                hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
            format!(".{hash:016x}")
        }
        Err(_) => String::new(),
    }
}

pub(super) fn window_title() -> Vec<u16> {
    wide(&format!("Pinfold{}", instance_key()))
}

pub(super) unsafe fn find_instance() -> HWND {
    FindWindowW(wide(WINDOW_CLASS).as_ptr(), window_title().as_ptr())
}

pub(super) fn quit_message() -> u32 {
    unsafe { RegisterWindowMessageW(wide("Pinfold.Quit").as_ptr()) }
}

/// Tell the background instance to stop and wait until it stops. Works
/// from a process without elevation also if the instance is elevated.
/// Returns false if it still runs after the timeout.
pub fn quit_running_instance() -> bool {
    unsafe {
        let hwnd = find_instance();
        if hwnd.is_null() {
            return true;
        }
        PostMessageW(hwnd, quit_message(), 0, 0);
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if find_instance().is_null() {
                return true;
            }
        }
        false
    }
}

pub fn is_running() -> bool {
    unsafe { !find_instance().is_null() }
}

/// Get the single-instance mutex. A previous instance that stops now
/// (reinstall, change to the elevated task) gets some seconds.
pub(super) unsafe fn acquire_single_instance() -> bool {
    let name = wide(&format!("Local\\Pinfold.Background{}", instance_key()));
    for _ in 0..25 {
        let handle = CreateMutexW(null(), 0, name.as_ptr());
        if !handle.is_null() && GetLastError() != ERROR_ALREADY_EXISTS {
            return true; // stays open until the process stops
        }
        if !handle.is_null() {
            CloseHandle(handle);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}
