//! Tray icon and menu. One thread: a window timer in the message loop also
//! runs the engine, thus there is no shared state to lock. The menu is
//! made when the user opens it. There are no background updates.
//!
//! - `menu`: the menu, its actions, About, Start with Windows
//! - `benchmark`: the benchmark thread, its state and its dialogs
//! - `icon`: the icon, the progress ring, notifications
//! - `instance`: one instance for each config directory, quit
//! - here: the hidden window, the message loop and the engine timer

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowW, GetCursorPos,
    GetMessageW, GetSystemMetrics, GetWindow, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsWindowVisible, KillTimer, LoadIconW, LoadImageW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, GW_OWNER, HMENU, IDI_APPLICATION, IMAGE_ICON, LR_SHARED, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, SM_CXSMICON, SM_CYSMICON,
    SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
};

use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GetShellWindow, HICON, ICONINFO,
};

use crate::autostart::{self, Kind};
use crate::bench;
use crate::config::{self, Mode};
use crate::sys::{process_info, RealSystem};
type Engine = crate::engine::Engine<RealSystem>;
use crate::dialog::{self, message_box, TaskDialog};
use crate::win::{file_name, from_wide, wide};

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    static INTERVAL: Cell<u32> = const { Cell::new(0) };
}
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static QUIT_MESSAGE: AtomicU32 = AtomicU32::new(0);
const WINDOW_CLASS: &str = "PinfoldTray";

mod benchmark;
mod icon;
mod instance;
mod menu;

use benchmark::*;
use icon::*;
use instance::*;
use menu::*;

pub use instance::{is_running, quit_running_instance};
pub use menu::{hidden_window, pick_profile};

/// Open a file or a URL with its default program.
fn open(path: &str) {
    unsafe {
        ShellExecuteW(
            null_mut(),
            wide("open").as_ptr(),
            wide(path).as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
    }
}

unsafe fn poll(hwnd: HWND) {
    let mut interval = 0;
    with_engine(|e| {
        e.tick();
        interval = e.cfg.poll_ms();
    });
    if interval != 0 && interval != INTERVAL.with(|i| i.get()) {
        INTERVAL.with(|i| i.set(interval));
        SetTimer(hwnd, TIMER_ID, interval, None);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            poll(hwnd);
            0
        }
        WM_BENCH_STATUS => {
            update_icon(hwnd, NIM_MODIFY);
            0
        }
        WM_BENCH_NOTE => {
            show_note(hwnd);
            0
        }
        WM_TRAY => {
            let event = lparam as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ID);
            // Put the rule back before the process stops.
            stop_benchmark();
            for _ in 0..30 {
                if !bench_running() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            crate::log::log("exit");
            PostQuitMessage(0);
            0
        }
        _ => {
            if msg != 0 && msg == QUIT_MESSAGE.load(Ordering::Relaxed) {
                DestroyWindow(hwnd);
                return 0;
            }
            // Explorer started again: add the icon again.
            if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) {
                update_icon(hwnd, NIM_ADD);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Background instance: tray icon and engine. Returns the exit code.
pub fn run() -> i32 {
    unsafe {
        // Started manually, and the elevated logon task exists: start the
        // task and stop. Thus Pinfold always runs with the rights of its
        // setup. (Not for instances with their own config directory, and
        // not if the task cannot run elevated.)
        if instance_key().is_empty()
            && crate::diag::has_elevated_twin()
            && autostart::task_exists()
            && autostart::run_task()
        {
            return 0;
        }
        if !acquire_single_instance() {
            message_box("Pinfold is already running. Look for its icon in the notification area.");
            return 0;
        }

        let engine = Engine::new(RealSystem, config::dir(), crate::topology::read());
        crate::diag::log_startup(&engine.topo);
        ENGINE.with(|e| *e.borrow_mut() = Some(engine));

        let hinstance = GetModuleHandleW(null());
        let class = wide(WINDOW_CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        // A normal top-level window (never shown), not a message-only
        // window. Only then does the TaskbarCreated broadcast arrive.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            window_title().as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        );
        if hwnd.is_null() {
            return 1;
        }
        TASKBAR_CREATED
            .store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
        // Let `Pinfold.exe quit` without elevation stop an elevated instance.
        let quit = quit_message();
        QUIT_MESSAGE.store(quit, Ordering::Relaxed);
        ChangeWindowMessageFilterEx(hwnd, quit, MSGFLT_ALLOW, null_mut());
        update_icon(hwnd, NIM_ADD);
        poll(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        0
    }
}
