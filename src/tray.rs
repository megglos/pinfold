//! Tray icon and menu. One thread: the message loop also drives the
//! engine through a window timer, so there is no shared state to lock.
//! The menu is built when opened; nothing is refreshed in the background.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
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

use windows_sys::Win32::UI::WindowsAndMessaging::GetShellWindow;

use crate::autostart::{self, Kind};
use crate::config::{self, Mode};
use crate::sys::{process_info, RealSystem};
type Engine = crate::engine::Engine<RealSystem>;
use crate::win::{file_name, from_wide, message_box, wide};

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    static INTERVAL: Cell<u32> = const { Cell::new(0) };
}
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static QUIT_MESSAGE: AtomicU32 = AtomicU32::new(0);
const WINDOW_CLASS: &str = "PinfoldTray";

enum Action {
    /// Create or update the file-name rule for this executable path.
    Assign {
        path: String,
        profile: String,
    },
    SetProfile {
        pattern: String,
        profile: String,
    },
    SetMode {
        pattern: String,
        mode: Mode,
    },
    ToggleEnforce {
        pattern: String,
    },
    Remove {
        pattern: String,
    },
    AddProgram,
    TogglePause,
    OpenConfig,
    OpenLog,
    ToggleAutostart,
    Exit,
    Nothing,
}

struct Menu {
    actions: Vec<Action>,
}

impl Menu {
    fn item(&mut self, menu: HMENU, label: &str, checked: bool, action: Action) {
        let mut flags = MF_STRING;
        if checked {
            flags |= MF_CHECKED;
        }
        if matches!(action, Action::Nothing) {
            flags |= MF_GRAYED;
        }
        self.actions.push(action);
        let label = wide(&label.replace('&', "&&"));
        unsafe { AppendMenuW(menu, flags, self.actions.len(), label.as_ptr()) };
    }
    fn sub(&mut self, menu: HMENU, label: &str, sub: HMENU) {
        let label = wide(&label.replace('&', "&&"));
        unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, label.as_ptr()) };
    }
    fn sep(&mut self, menu: HMENU) {
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, null()) };
    }
}

/// Shows the menu at the cursor and returns the chosen action.
unsafe fn track(hwnd: HWND, hmenu: HMENU, mut m: Menu) -> Action {
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        hmenu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        null(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(hmenu);
    if cmd > 0 && (cmd as usize) <= m.actions.len() {
        m.actions.swap_remove(cmd as usize - 1)
    } else {
        Action::Nothing
    }
}

/// Popup with the profile list. Used by "Add program..." and by the
/// short-lived drop instance.
pub unsafe fn pick_profile(hwnd: HWND, exe_name: &str, profiles: &[String]) -> Option<String> {
    let mut m = Menu { actions: Vec::new() };
    let menu = CreatePopupMenu();
    m.item(menu, &format!("Pin {exe_name} to:"), false, Action::Nothing);
    m.sep(menu);
    for p in profiles {
        m.item(menu, p, false, Action::Assign { path: exe_name.to_string(), profile: p.clone() });
    }
    match track(hwnd, menu, m) {
        Action::Assign { profile, .. } => Some(profile),
        _ => None,
    }
}

/// Hidden helper window for instances that only need a popup menu.
pub unsafe fn hidden_window() -> HWND {
    CreateWindowExW(
        0,
        wide("STATIC").as_ptr(),
        wide("Pinfold").as_ptr(),
        0,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    )
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
    let pids = &mut *(lparam as *mut Vec<u32>);
    if IsWindowVisible(hwnd) != 0
        && GetWindow(hwnd, GW_OWNER).is_null()
        && GetWindowTextLengthW(hwnd) > 0
    {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 && pid != std::process::id() && !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    1
}

/// Order of the Assign list. `z_order` holds (path, start time) of the
/// processes owning visible windows, topmost window first. The program the
/// user was just using comes first: the topmost one that is not the shell
/// (clicking the tray icon puts the taskbar in front). The rest follows,
/// most recently started first, one entry per executable path.
fn assign_order(z_order: Vec<(String, u64)>, shell: Option<&str>, limit: usize) -> Vec<String> {
    let is_shell = |path: &str| shell.is_some_and(|s| s.eq_ignore_ascii_case(path));
    let front = z_order.iter().find(|(path, _)| !is_shell(path)).map(|(path, _)| path.clone());
    let mut rest = z_order;
    rest.sort_by_key(|p| std::cmp::Reverse(p.1));
    let mut out: Vec<String> = Vec::new();
    for path in front.into_iter().chain(rest.into_iter().map(|(path, _)| path)) {
        if !out.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
            out.push(path);
        }
    }
    out.truncate(limit);
    out
}

/// Processes that own a visible top-level window, in Assign order.
/// Computed when the menu opens; nothing tracks the foreground window in
/// the background (EnumWindows reports windows in Z-order).
fn windowed_processes() -> Vec<String> {
    let mut pids: Vec<u32> = Vec::new();
    unsafe { EnumWindows(Some(enum_windows_cb), &mut pids as *mut Vec<u32> as LPARAM) };
    let z_order: Vec<(String, u64)> = pids.into_iter().filter_map(process_info).collect();
    let mut shell_pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetShellWindow(), &mut shell_pid) };
    let shell = process_info(shell_pid).map(|(path, _)| path);
    assign_order(z_order, shell.as_deref(), 30)
}
fn build_menu(e: &Engine) -> (HMENU, Menu) {
    let mut m = Menu { actions: Vec::new() };
    unsafe {
        let root = CreatePopupMenu();

        // Assign > running windowed processes > profiles
        let assign = CreatePopupMenu();
        for path in windowed_processes() {
            let name = file_name(&path).to_string();
            let current = e.cfg.rule_index(&name).map(|i| e.cfg.rules[i].profile.clone());
            let sub = CreatePopupMenu();
            for p in &e.cfg.profiles {
                let checked = current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&p.name));
                m.item(
                    sub,
                    &format!("{}  (cpus {})", p.name, p.cpus),
                    checked,
                    Action::Assign { path: path.clone(), profile: p.name.clone() },
                );
            }
            let label = match &current {
                Some(c) => format!("{name}  \u{2192} {c}"),
                None => name,
            };
            m.sub(assign, &label, sub);
        }
        m.sub(root, "Assign", assign);

        // Rules > rule > profile / mode / enforce / remove
        let rules = CreatePopupMenu();
        if e.cfg.rules.is_empty() {
            m.item(rules, "(no rules yet)", false, Action::Nothing);
        }
        for (i, r) in e.cfg.rules.iter().enumerate() {
            let (state, count) = e.rule_status(i);
            let sub = CreatePopupMenu();
            for p in &e.cfg.profiles {
                m.item(
                    sub,
                    &format!("{}  (cpus {})", p.name, p.cpus),
                    p.name.eq_ignore_ascii_case(&r.profile),
                    Action::SetProfile { pattern: r.pattern.clone(), profile: p.name.clone() },
                );
            }
            m.sep(sub);
            for mode in [Mode::Affinity, Mode::Cpusets, Mode::Both] {
                m.item(
                    sub,
                    &format!("Mode: {}", mode.as_str()),
                    r.mode == mode,
                    Action::SetMode { pattern: r.pattern.clone(), mode },
                );
            }
            m.sep(sub);
            m.item(sub, "Enforce", r.enforce, Action::ToggleEnforce { pattern: r.pattern.clone() });
            m.item(sub, "Remove rule", false, Action::Remove { pattern: r.pattern.clone() });
            let status = if count == 0 {
                "not running".to_string()
            } else {
                format!("{}, {count} running", state.as_str())
            };
            m.sub(rules, &format!("{}  \u{2192} {}  [{status}]", r.pattern, r.profile), sub);
        }
        m.sub(root, "Rules", rules);
        m.item(root, "Add program...", false, Action::AddProgram);
        m.sep(root);
        m.item(root, "Pause", e.paused, Action::TogglePause);
        m.item(root, "Open config", false, Action::OpenConfig);
        m.item(root, "Open log", false, Action::OpenLog);
        m.item(
            root,
            "Start with Windows",
            autostart::status() != Kind::Off,
            Action::ToggleAutostart,
        );
        m.sep(root);
        m.item(root, "Exit", false, Action::Exit);
        (root, m)
    }
}

unsafe fn show_menu(hwnd: HWND) {
    // The borrow must not be held while the menu is open: the poll timer
    // keeps firing inside the menu's modal loop.
    let built = ENGINE.with(|e| e.borrow().as_ref().map(build_menu));
    let Some((hmenu, menu)) = built else { return };
    let action = track(hwnd, hmenu, menu);
    handle(hwnd, action);
}

fn with_engine(f: impl FnOnce(&mut Engine)) {
    ENGINE.with(|e| {
        if let Ok(mut guard) = e.try_borrow_mut() {
            if let Some(engine) = guard.as_mut() {
                f(engine);
            }
        }
    });
}

unsafe fn handle(hwnd: HWND, action: Action) {
    match action {
        Action::Nothing => {}
        Action::Assign { path, profile } => with_engine(|e| {
            e.cfg.upsert_rule(file_name(&path), &profile, None);
            e.save_and_apply();
        }),
        Action::SetProfile { pattern, profile } => with_engine(|e| {
            if let Some(i) = e.cfg.rule_index(&pattern) {
                e.cfg.rules[i].profile = profile;
                e.save_and_apply();
            }
        }),
        Action::SetMode { pattern, mode } => with_engine(|e| {
            if let Some(i) = e.cfg.rule_index(&pattern) {
                e.cfg.rules[i].mode = mode;
                e.save_and_apply();
            }
        }),
        Action::ToggleEnforce { pattern } => with_engine(|e| {
            if let Some(i) = e.cfg.rule_index(&pattern) {
                e.cfg.rules[i].enforce = !e.cfg.rules[i].enforce;
                e.save_and_apply();
            }
        }),
        Action::Remove { pattern } => with_engine(|e| {
            if let Some(i) = e.cfg.rule_index(&pattern) {
                e.cfg.rules.remove(i);
                e.save_and_apply();
            }
        }),
        Action::AddProgram => add_program(hwnd),
        Action::TogglePause => {
            with_engine(|e| e.set_paused(!e.paused));
            update_icon(hwnd, NIM_MODIFY);
        }
        Action::OpenConfig => open(&config::path(&config::dir()).to_string_lossy()),
        Action::OpenLog => open(&config::log_path(&config::dir()).to_string_lossy()),
        Action::ToggleAutostart => toggle_autostart(hwnd),
        Action::Exit => {
            DestroyWindow(hwnd);
        }
    }
}

unsafe fn add_program(hwnd: HWND) {
    let mut file = [0u16; 1024];
    let filter: Vec<u16> = "Programs and shortcuts\0*.exe;*.lnk\0\0".encode_utf16().collect();
    let title = wide("Add program to Pinfold");
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR;
    if GetOpenFileNameW(&mut ofn) == 0 {
        return;
    }
    let path = from_wide(&file);
    let name = file_name(&path).to_string();
    let profiles: Vec<String> = ENGINE.with(|e| {
        e.borrow()
            .as_ref()
            .map(|e| e.cfg.profiles.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    });
    if let Some(profile) = pick_profile(hwnd, &name, &profiles) {
        with_engine(|e| {
            e.cfg.upsert_rule(&name, &profile, None);
            e.save_and_apply();
        });
    }
}

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

// --- Start with Windows -----------------------------------------------------

unsafe fn toggle_autostart(hwnd: HWND) {
    let elevated =
        ENGINE.with(|e| e.borrow().as_ref().map(|e| e.cfg.elevated_autostart)).unwrap_or(true);
    let result = if autostart::status() == Kind::Off {
        autostart::enable(elevated)
    } else {
        autostart::disable()
    };
    match result {
        Err(e) => message_box(&e),
        Ok(kind) => {
            crate::log::log(&format!("autostart {}", kind.describe()));
            // Switch to the elevated instance right away instead of at the
            // next logon: start the task and make room for it.
            if kind == Kind::Elevated && !crate::diag::is_elevated() && autostart::run_task() {
                DestroyWindow(hwnd);
            }
        }
    }
}

// --- Single instance and quit -----------------------------------------------

/// Distinguishes instances that use different config directories (tests);
/// empty for the normal instance.
fn instance_key() -> String {
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

fn window_title() -> Vec<u16> {
    wide(&format!("Pinfold{}", instance_key()))
}

unsafe fn find_instance() -> HWND {
    FindWindowW(wide(WINDOW_CLASS).as_ptr(), window_title().as_ptr())
}

fn quit_message() -> u32 {
    unsafe { RegisterWindowMessageW(wide("Pinfold.Quit").as_ptr()) }
}

/// Ask the background instance to exit and wait until it is gone.
/// Works from an unelevated process even when the instance is elevated.
/// Returns false when it is still running after the timeout.
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

/// Take the single-instance mutex. A previous instance that is just
/// exiting (reinstall, switch to the elevated task) gets a few seconds.
unsafe fn acquire_single_instance() -> bool {
    let name = wide(&format!("Local\\Pinfold.Background{}", instance_key()));
    for _ in 0..25 {
        let handle = CreateMutexW(null(), 0, name.as_ptr());
        if !handle.is_null() && GetLastError() != ERROR_ALREADY_EXISTS {
            return true; // kept open for the lifetime of the process
        }
        if !handle.is_null() {
            CloseHandle(handle);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

// --- Icon, window, message loop --------------------------------------------

unsafe fn update_icon(hwnd: HWND, message: u32) {
    let paused = ENGINE.with(|e| e.try_borrow().ok().and_then(|g| g.as_ref().map(|e| e.paused)));
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    // Embedded icons: 1 = normal, 2 = paused (see build.rs).
    let id: usize = if paused == Some(true) { 2 } else { 1 };
    nid.hIcon = LoadImageW(
        GetModuleHandleW(null()),
        id as *const u16,
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_SHARED,
    );
    if nid.hIcon.is_null() {
        nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    }
    let tip = wide(if paused == Some(true) { "Pinfold (paused)" } else { "Pinfold" });
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    Shell_NotifyIconW(message, &nid);
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
        WM_TRAY => {
            let event = lparam as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ID);
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            Shell_NotifyIconW(NIM_DELETE, &nid);
            crate::log::log("exit");
            PostQuitMessage(0);
            0
        }
        _ => {
            if msg != 0 && msg == QUIT_MESSAGE.load(Ordering::Relaxed) {
                DestroyWindow(hwnd);
                return 0;
            }
            // Explorer restarted: the icon has to be added again.
            if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) {
                update_icon(hwnd, NIM_ADD);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Background instance: tray icon plus engine. Returns the exit code.
pub fn run() -> i32 {
    unsafe {
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
        // A regular (never shown) top-level window, not a message-only one,
        // so that the TaskbarCreated broadcast arrives.
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
        // Let an unelevated `Pinfold.exe quit` reach an elevated instance.
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

#[cfg(test)]
mod tests {
    use super::assign_order;

    fn z(items: &[(&str, u64)]) -> Vec<(String, u64)> {
        items.iter().map(|(p, s)| (p.to_string(), *s)).collect()
    }

    const SHELL: &str = "C:\\Windows\\explorer.exe";

    #[test]
    fn program_in_front_comes_first_then_most_recently_started() {
        let procs = z(&[
            ("D:\\game.exe", 100),
            ("C:\\old.exe", 50),
            ("C:\\newest.exe", 300),
            ("C:\\mid.exe", 200),
        ]);
        let order = assign_order(procs, Some(SHELL), 30);
        assert_eq!(order, ["D:\\game.exe", "C:\\newest.exe", "C:\\mid.exe", "C:\\old.exe"]);
    }

    #[test]
    fn shell_on_top_is_skipped_as_foreground() {
        // After a click on the tray icon the taskbar (explorer) is topmost.
        let procs = z(&[(SHELL, 10), ("D:\\game.exe", 100), ("C:\\newer.exe", 300)]);
        let order = assign_order(procs, Some("c:\\windows\\EXPLORER.EXE"), 30);
        assert_eq!(order, ["D:\\game.exe", "C:\\newer.exe", SHELL]);
    }

    #[test]
    fn one_entry_per_executable_and_limit() {
        let procs = z(&[("C:\\a.exe", 1), ("c:\\A.EXE", 5), ("C:\\b.exe", 3), ("C:\\c.exe", 2)]);
        assert_eq!(assign_order(procs.clone(), None, 30), ["C:\\a.exe", "C:\\b.exe", "C:\\c.exe"]);
        assert_eq!(assign_order(procs, None, 2).len(), 2);
    }

    #[test]
    fn empty_and_shell_only() {
        assert!(assign_order(Vec::new(), Some(SHELL), 30).is_empty());
        assert_eq!(assign_order(z(&[(SHELL, 1)]), Some(SHELL), 30), [SHELL]);
    }
}
