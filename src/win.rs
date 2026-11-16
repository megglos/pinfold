//! Small Win32 helpers shared by all modules.

use std::ptr::null;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS,
    STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
};

/// UTF-16, NUL-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Lossy conversion of a UTF-16 buffer up to the first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// The binary uses the GUI subsystem so the tray instance never flashes a
/// console. For command-line use, attach to the parent's console unless
/// stdout is already redirected (pipe or file).
pub fn console_init() {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let con = CreateFileW(
            wide("CONOUT$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if con != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, con);
            SetStdHandle(STD_ERROR_HANDLE, con);
        }
    }
}

/// Open an own console window (for a command started from the tray menu,
/// where there is no parent console) and connect the standard handles.
pub fn console_window(title: &str) {
    use windows_sys::Win32::System::Console::SetConsoleTitleW;
    unsafe {
        if AllocConsole() == 0 {
            return;
        }
        SetConsoleTitleW(wide(title).as_ptr());
        let open = |name: &str| {
            CreateFileW(
                wide(name).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        let out = open("CONOUT$");
        if out != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, out);
            SetStdHandle(STD_ERROR_HANDLE, out);
        }
        let input = open("CONIN$");
        if input != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_INPUT_HANDLE, input);
        }
    }
}

/// Keep a console window open until the user presses Enter.
pub fn wait_for_enter() {
    say("\nPress Enter to close this window.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Print a line to stdout, ignoring a missing or broken stdout.
pub fn say(s: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{s}");
}

pub fn message_box(text: &str) {
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide("Pinfold").as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        );
    }
}

/// File name part of a Windows path.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}
