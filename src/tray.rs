//! Tray icon and menu. One thread: a window timer in the message loop also
//! runs the engine, thus there is no shared state to lock. The menu is
//! made when the user opens it. There are no background updates.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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
    IsWindowVisible, KillTimer, LoadIconW, LoadImageW, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, GW_OWNER, HMENU, IDI_APPLICATION, IDYES, IMAGE_ICON, LR_SHARED,
    MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, SM_CXSMICON, SM_CYSMICON, SW_SHOWNORMAL,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
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
    /// Make or update the file-name rule for this executable path.
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
    /// Compare profiles for a running program with PresentMon.
    Benchmark {
        pattern: String,
        profiles: Vec<String>,
    },
    StopBenchmark,
    OpenReport {
        path: String,
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

/// Shows the menu at the cursor and returns the selected action.
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

/// Popup with the profiles. For "Add program..." and for the short drop
/// instance.
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

/// Hidden helper window for instances that need only a popup menu.
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

/// Order of the Assign list. `z_order` has (path, start time) of the
/// processes with visible windows, topmost window first. First comes the
/// program that the user used last: the topmost one that is not the shell
/// (a click on the tray icon puts the taskbar in front). Then the others,
/// newest first, one entry for each executable path.
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

/// Processes with a visible top-level window, in Assign order. Calculated
/// when the menu opens. Nothing monitors the foreground window in the
/// background (EnumWindows gives windows in Z-order).
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
    let running = bench_running();
    unsafe {
        let root = CreatePopupMenu();
        if running {
            let status = BENCH_STATUS.lock().ok().and_then(|s| s.clone()).unwrap_or_default();
            m.item(root, &format!("Stop benchmark  ({status})"), false, Action::StopBenchmark);
            m.sep(root);
        }

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

        // Benchmark > comparisons for each running program with a rule
        if !running {
            let names: Vec<String> = e.cfg.profiles.iter().map(|p| p.name.clone()).collect();
            let targets: Vec<&config::Rule> = (0..e.cfg.rules.len())
                .filter(|&i| e.rule_status(i).1 > 0)
                .map(|i| &e.cfg.rules[i])
                .collect();
            if targets.is_empty() {
                m.item(root, "Benchmark (start a program with a rule)", false, Action::Nothing);
            } else {
                let bench = CreatePopupMenu();
                for (n, r) in targets.iter().enumerate() {
                    if n > 0 {
                        m.sep(bench);
                    }
                    m.item(
                        bench,
                        &format!("{}  (now {})", r.pattern, r.profile),
                        false,
                        Action::Nothing,
                    );
                    for choice in benchmark_choices(&names, &r.profile) {
                        let label = format!("    {}", choice_label(&choice, names.len()));
                        let action =
                            Action::Benchmark { pattern: r.pattern.clone(), profiles: choice };
                        m.item(bench, &label, false, action);
                    }
                }
                m.sub(root, "Benchmark", bench);
            }
        }
        m.item(root, "Pause", e.paused, Action::TogglePause);
        m.item(root, "Open config", false, Action::OpenConfig);
        m.item(root, "Open log", false, Action::OpenLog);
        if let Some(report) = last_report() {
            let path = report.to_string_lossy().into_owned();
            m.item(root, "Open last benchmark report", false, Action::OpenReport { path });
        }
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
    // Do not keep the borrow while the menu is open: the poll timer
    // continues in the modal loop of the menu.
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
        Action::Benchmark { pattern, profiles } => {
            start_benchmark(hwnd, file_name(&pattern).to_string(), profiles)
        }
        Action::StopBenchmark => stop_benchmark(),
        Action::OpenReport { path } => open(&path),
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

// --- Benchmark without a window ----------------------------------------------
//
// The benchmark runs on a separate thread in this process. Thus it has the
// elevation of this instance (PresentMon needs it), and the engine on the
// main thread applies each profile switch. The tooltip, the icon and the
// log show the progress. Sounds give signals. A notification and a dialog
// show the result.

/// Set while a benchmark runs. Store true to stop it.
static BENCH_CANCEL: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
/// Progress for the tooltip.
static BENCH_STATUS: Mutex<Option<String>> = Mutex::new(None);
/// Share of the captures done, in thousandths.
static BENCH_PROGRESS: AtomicU32 = AtomicU32::new(0);
/// A notification for the main thread (title, text).
static BENCH_NOTE: Mutex<Option<(String, String)>> = Mutex::new(None);
const WM_BENCH_STATUS: u32 = WM_APP + 2;
const WM_BENCH_NOTE: u32 = WM_APP + 3;

fn bench_running() -> bool {
    BENCH_CANCEL.lock().map(|c| c.is_some()).unwrap_or(false)
}

/// Comparisons for a rule: Cache and Frequency, the profile of the rule and
/// each other profile, and all profiles.
fn benchmark_choices(profiles: &[String], current: &str) -> Vec<Vec<String>> {
    let find = |name: &str| profiles.iter().find(|p| p.eq_ignore_ascii_case(name)).cloned();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut add = |choice: Vec<String>| {
        let key = |c: &[String]| {
            let mut k: Vec<String> = c.iter().map(|p| p.to_lowercase()).collect();
            k.sort();
            k
        };
        if choice.len() >= 2 && !out.iter().any(|c| key(c) == key(&choice)) {
            out.push(choice);
        }
    };
    if let (Some(c), Some(f)) = (find("Cache"), find("Frequency")) {
        add(vec![c, f]);
    }
    if let Some(cur) = find(current) {
        for p in profiles.iter().filter(|p| !p.eq_ignore_ascii_case(&cur)) {
            add(vec![cur.clone(), p.clone()]);
        }
    }
    if profiles.len() > 2 {
        add(profiles.to_vec());
    }
    out
}

fn choice_label(choice: &[String], all: usize) -> String {
    if choice.len() > 2 && choice.len() == all {
        format!("All {all} profiles")
    } else {
        choice.join(" vs ")
    }
}

/// The report of the last benchmark.
fn last_report() -> Option<std::path::PathBuf> {
    let dir = config::dir().join("benchmarks");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path().join("report.txt"))
        .filter_map(|p| std::fs::metadata(&p).ok()?.modified().ok().map(|t| (t, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

struct TrayUi {
    hwnd: usize,
}

impl bench::Ui for TrayUi {
    fn say(&self, line: &str) {
        crate::log::log(&format!("benchmark: {line}"));
    }
    fn status(&self, text: &str) {
        if let Ok(mut s) = BENCH_STATUS.lock() {
            *s = Some(text.to_string());
        }
        unsafe { PostMessageW(self.hwnd as HWND, WM_BENCH_STATUS, 0, 0) };
    }
    fn sound(&self, sound: bench::Sound) {
        bench::play(sound);
    }
    fn progress(&self, done: f64) {
        BENCH_PROGRESS.store((done.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }
}

fn notify(hwnd: usize, title: &str, text: &str) {
    if let Ok(mut n) = BENCH_NOTE.lock() {
        *n = Some((title.to_string(), text.to_string()));
    }
    unsafe { PostMessageW(hwnd as HWND, WM_BENCH_NOTE, 0, 0) };
}

fn ask(text: &str) -> bool {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(text).as_ptr(),
            wide("Pinfold benchmark").as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn tell(text: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(text).as_ptr(),
            wide("Pinfold benchmark").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

const BUTTON_APPLY: i32 = 100;
const BUTTON_REPORT: i32 = 101;
const BUTTON_CLOSE: i32 = 102;

/// The benchmark result. One button opens the report and the dialog stays
/// open. If another profile won, one button makes it the rule. Returns true
/// if the winner must become the rule.
fn result_dialog(heading: &str, text: &str, report: &str, apply: Option<(&str, &str)>) -> bool {
    use windows_sys::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
        TDN_BUTTON_CLICKED, TD_INFORMATION_ICON,
    };

    unsafe extern "system" fn callback(
        _hwnd: HWND,
        msg: i32,
        wparam: WPARAM,
        _lparam: LPARAM,
        data: isize,
    ) -> i32 {
        if msg == TDN_BUTTON_CLICKED && wparam as i32 == BUTTON_REPORT {
            open(&*(data as *const String));
            return 1; // S_FALSE: keep the dialog open
        }
        0
    }

    let labels: Vec<(i32, String)> = match apply {
        Some((winner, current)) => vec![
            (BUTTON_APPLY, format!("Make {winner} the rule")),
            (BUTTON_REPORT, "Open report".to_string()),
            (BUTTON_CLOSE, format!("Keep {current}")),
        ],
        None => {
            vec![(BUTTON_REPORT, "Open report".to_string()), (BUTTON_CLOSE, "Close".to_string())]
        }
    };
    let wide_labels: Vec<Vec<u16>> = labels.iter().map(|(_, l)| wide(l)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = labels
        .iter()
        .zip(&wide_labels)
        .map(|((id, _), l)| TASKDIALOG_BUTTON { nButtonID: *id, pszButtonText: l.as_ptr() })
        .collect();
    let title = wide("Pinfold benchmark");
    let heading_w = wide(heading);
    let text_w = wide(text);
    let report = report.to_string();

    let mut pressed = 0i32;
    unsafe {
        let mut cfg: TASKDIALOGCONFIG = std::mem::zeroed();
        cfg.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
        cfg.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION;
        cfg.pszWindowTitle = title.as_ptr();
        cfg.Anonymous1.pszMainIcon = TD_INFORMATION_ICON;
        cfg.pszMainInstruction = heading_w.as_ptr();
        cfg.pszContent = text_w.as_ptr();
        cfg.cButtons = buttons.len() as u32;
        cfg.pButtons = buttons.as_ptr();
        cfg.nDefaultButton = buttons[0].nButtonID;
        cfg.pfCallback = Some(callback);
        cfg.lpCallbackData = &report as *const String as isize;
        if TaskDialogIndirect(&cfg, &mut pressed, null_mut(), null_mut()) < 0 {
            // No task dialog: use a message box.
            let mut plain = format!("{heading}\n\n{text}\n\nReport: {report}");
            return match apply {
                Some((winner, _)) => {
                    plain.push_str(&format!("\n\nMake {winner} the rule?"));
                    ask(&plain)
                }
                None => {
                    tell(&plain);
                    false
                }
            };
        }
    }
    pressed == BUTTON_APPLY
}

fn start_benchmark(hwnd: HWND, target: String, profiles: Vec<String>) {
    let paused = ENGINE.with(|e| e.borrow().as_ref().map(|e| e.paused)).unwrap_or(false);
    if paused {
        message_box("Pinfold is paused, so it cannot switch profiles. Resume it first.");
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    BENCH_PROGRESS.store(0, Ordering::Relaxed);
    match BENCH_CANCEL.lock() {
        Ok(mut c) if c.is_none() => *c = Some(cancel.clone()),
        _ => return,
    }
    let hwnd = hwnd as usize;
    notify(
        hwnd,
        &format!("Benchmark of {target}: switch to it"),
        "It starts once the program is in front. Hover the Pinfold icon for progress; \
         right-click it to stop.",
    );
    let args = [target.clone(), "--profiles".to_string(), profiles.join(",")];
    std::thread::spawn(move || {
        let ui = TrayUi { hwnd };
        let result = bench::parse_args(&args)
            .map_err(bench::Failure::from)
            .and_then(|opts| bench::run(&opts, &ui, &cancel));
        if let Ok(mut c) = BENCH_CANCEL.lock() {
            *c = None;
        }
        if let Ok(mut s) = BENCH_STATUS.lock() {
            *s = None;
        }
        unsafe { PostMessageW(hwnd as HWND, WM_BENCH_STATUS, 0, 0) };
        finish_benchmark(hwnd, &target, result);
    });
}

/// Show the result. Runs on the benchmark thread, thus the dialogs do not
/// block the tray.
fn finish_benchmark(hwnd: usize, target: &str, result: Result<bench::Outcome, bench::Failure>) {
    match result {
        Ok(outcome) => {
            crate::log::log(&format!("benchmark: {}", outcome.verdict.text));
            let title = match &outcome.verdict.winner {
                Some(w) => format!("Benchmark of {target}: {w} wins"),
                None => format!("Benchmark of {target}: no clear winner"),
            };
            notify(hwnd, &title, &outcome.verdict.text);
            let heading = match &outcome.verdict.winner {
                Some(w) => format!("{w} wins for {target}"),
                None => format!("No clear winner for {target}"),
            };
            let mut text = format!("{}\n\n{}", outcome.verdict.text, outcome.summary);
            if let Some(note) = &outcome.gpu_note {
                text.push_str(&format!("\n{note}"));
            }
            let better = outcome.better_profile();
            if better.is_none() {
                if let Some(w) = &outcome.verdict.winner {
                    text.push_str(&format!("\n\n{target} already uses {w}."));
                }
            }
            let report = outcome.report_path.to_string_lossy().into_owned();
            let current = outcome.current.as_deref().unwrap_or("the current profile");
            let apply =
                better.filter(|w| result_dialog(&heading, &text, &report, Some((w, current))));
            if better.is_none() {
                result_dialog(&heading, &text, &report, None);
            }
            if let Some(w) = apply {
                if let Err(e) = bench::apply(target, w) {
                    tell(&format!("Could not change the rule: {e}"));
                }
            }
        }
        Err(bench::Failure::Cancelled) => {
            crate::log::log("benchmark: stopped");
            notify(hwnd, "Benchmark stopped", "The rule is as it was before.");
        }
        Err(bench::Failure::NeedsElevation) => {
            let text = "PresentMon needs administrator rights, and this Pinfold runs without \
                        them.\n\nRestart Pinfold with administrator rights? Then start the \
                        benchmark again from the menu.";
            if ask(text) {
                let started = std::env::current_exe()
                    .map_err(|e| e.to_string())
                    .and_then(|exe| crate::win::launch_elevated(&exe, &[]));
                match started {
                    // The new instance waits until this one stops.
                    Ok(()) => unsafe {
                        PostMessageW(hwnd as HWND, quit_message(), 0, 0);
                    },
                    Err(e) => tell(&e),
                }
            }
        }
        Err(bench::Failure::Message(e)) => {
            crate::log::log(&format!("benchmark failed: {e}"));
            notify(hwnd, "Benchmark stopped", "The rule is as it was before.");
            tell(&format!("Benchmark stopped: {e}\n\nThe rule is as it was before."));
        }
    }
}

fn stop_benchmark() {
    if let Ok(c) = BENCH_CANCEL.lock() {
        if let Some(cancel) = c.as_ref() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Copy text into a UTF-16 field of fixed size. Cut it to fit with its NUL.
fn fill(field: &mut [u16], text: &str) {
    let mut w: Vec<u16> = text.encode_utf16().take(field.len() - 1).collect();
    w.push(0);
    field[..w.len()].copy_from_slice(&w);
}

unsafe fn show_note(hwnd: HWND) {
    let Some((title, text)) = BENCH_NOTE.lock().ok().and_then(|mut n| n.take()) else {
        return;
    };
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    fill(&mut nid.szInfoTitle, &title);
    fill(&mut nid.szInfo, &text);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
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
            // Change to the elevated instance now, not at the next logon:
            // start the task and stop this instance.
            if kind == Kind::Elevated && !crate::diag::is_elevated() && autostart::run_task() {
                DestroyWindow(hwnd);
            }
        }
    }
}

// --- Single instance and quit -----------------------------------------------

/// Identifies instances with different config directories (tests). Empty
/// for the normal instance.
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
unsafe fn acquire_single_instance() -> bool {
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

// --- Icon, window, message loop --------------------------------------------

unsafe fn update_icon(hwnd: HWND, message: u32) {
    let paused = ENGINE.with(|e| e.try_borrow().ok().and_then(|g| g.as_ref().map(|e| e.paused)));
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    let status = BENCH_STATUS.lock().ok().and_then(|s| s.clone());
    // While a benchmark runs: a ring that fills with the progress.
    let ring = match status {
        Some(_) => progress_icon(BENCH_PROGRESS.load(Ordering::Relaxed) as f64 / 1000.0),
        None => null_mut(),
    };
    if ring.is_null() {
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
    } else {
        nid.hIcon = ring;
    }
    if nid.hIcon.is_null() {
        nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    }
    let tip = match status {
        Some(s) => format!("Pinfold benchmark: {s}"),
        None if paused == Some(true) => "Pinfold (paused)".to_string(),
        None => "Pinfold".to_string(),
    };
    fill(&mut nid.szTip, &tip);
    Shell_NotifyIconW(message, &nid);
    // The shell keeps its own copy. Destroy the previous ring.
    let old = RING_ICON.with(|r| r.replace(ring));
    if !old.is_null() {
        DestroyIcon(old);
    }
}

thread_local! {
    static RING_ICON: Cell<HICON> = const { Cell::new(null_mut()) };
}

/// Pixels (ARGB, rows from the top) of a progress ring: a track, the done
/// part clockwise from 12 o'clock, and a dot in the middle.
fn ring_pixels(size: usize, done: f64) -> Vec<u32> {
    const LIT: u32 = 0x1E90FF; // blue
    const TRACK: u32 = 0x9A9A9A; // grey, half transparent
    const SAMPLES: usize = 4; // for each axis, for smooth edges
    let c = size as f64 / 2.0;
    let (outer, inner, dot) = (c - 0.5, c * 0.58, c * 0.28);
    let mut out = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let (mut lit, mut track) = (0usize, 0usize);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f64 + (sx as f64 + 0.5) / SAMPLES as f64 - c;
                    let py = y as f64 + (sy as f64 + 0.5) / SAMPLES as f64 - c;
                    let r = (px * px + py * py).sqrt();
                    if r <= dot {
                        lit += 1;
                    } else if r >= inner && r <= outer {
                        // Angle clockwise from the top, 0 to 1.
                        let a = px.atan2(-py) / std::f64::consts::TAU;
                        if a.rem_euclid(1.0) < done {
                            lit += 1;
                        } else {
                            track += 1;
                        }
                    }
                }
            }
            let n = (SAMPLES * SAMPLES) as f64;
            let alpha_lit = lit as f64 / n * 255.0;
            let alpha_track = track as f64 / n * 150.0;
            let (rgb, alpha) = if alpha_lit >= alpha_track {
                (LIT, alpha_lit + alpha_track * 0.3)
            } else {
                (TRACK, alpha_track)
            };
            out.push(((alpha.min(255.0) as u32) << 24) | rgb);
        }
    }
    out
}

/// A tray icon with the progress ring. Null if it fails.
unsafe fn progress_icon(done: f64) -> HICON {
    let size = GetSystemMetrics(SM_CXSMICON).max(16) as usize;
    let pixels = ring_pixels(size, done);
    let color = CreateBitmap(size as i32, size as i32, 1, 32, pixels.as_ptr().cast());
    let mask_bits = vec![0u8; size.div_ceil(16) * 2 * size];
    let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr().cast());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon =
        if color.is_null() || mask.is_null() { null_mut() } else { CreateIconIndirect(&info) };
    DeleteObject(color);
    DeleteObject(mask);
    icon
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

#[cfg(test)]
mod tests {
    use super::{assign_order, ring_pixels};

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
        // After a click on the tray icon, the taskbar (explorer) is topmost.
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

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn benchmark_choices_for_the_default_profiles() {
        let profiles = names(&["Cache", "Frequency", "All cores"]);
        let choices = super::benchmark_choices(&profiles, "cache");
        let labels: Vec<String> = choices.iter().map(|c| super::choice_label(c, 3)).collect();
        assert_eq!(labels, ["Cache vs Frequency", "Cache vs All cores", "All 3 profiles"]);

        // A rule with another profile: compare it with each other profile.
        let choices = super::benchmark_choices(&profiles, "All cores");
        let labels: Vec<String> = choices.iter().map(|c| super::choice_label(c, 3)).collect();
        assert_eq!(
            labels,
            [
                "Cache vs Frequency",
                "All cores vs Cache",
                "All cores vs Frequency",
                "All 3 profiles"
            ]
        );
    }

    #[test]
    fn benchmark_choices_for_own_profiles() {
        let choices = super::benchmark_choices(&names(&["Fast", "Slow"]), "Slow");
        assert_eq!(choices, [names(&["Slow", "Fast"])]);
        assert!(super::benchmark_choices(&names(&["Only"]), "Only").is_empty());
        // If the profile of a rule does not exist, offer all profiles.
        let choices = super::benchmark_choices(&names(&["A", "B", "C"]), "Gone");
        assert_eq!(choices, [names(&["A", "B", "C"])]);
    }

    #[test]
    fn progress_ring_fills_clockwise_from_the_top() {
        let size = 16;
        let px = |p: &[u32], x: usize, y: usize| p[y * size + x];
        let lit = |v: u32| v & 0xFFFFFF == 0x1E90FF && v >> 24 > 128;
        let track = |v: u32| v & 0xFFFFFF == 0x9A9A9A && v >> 24 > 64;
        // Right (3 o'clock), bottom, left of the ring, and the middle.
        let (right, bottom, left) = ((14, 8), (8, 14), (1, 8));
        let quarter = ring_pixels(size, 0.3);
        assert!(lit(px(&quarter, right.0, right.1)));
        assert!(track(px(&quarter, bottom.0, bottom.1)));
        assert!(track(px(&quarter, left.0, left.1)));
        assert!(lit(px(&quarter, 8, 8)), "the dot is always drawn");
        let most = ring_pixels(size, 0.8);
        assert!(lit(px(&most, left.0, left.1)));
        let none = ring_pixels(size, 0.0);
        assert!(track(px(&none, right.0, right.1)));
        assert_eq!(px(&none, 0, 0) >> 24, 0, "the corners are transparent");
    }

    #[test]
    fn long_text_is_cut_to_fit_the_field() {
        let mut field = [1u16; 8];
        super::fill(&mut field, "Pinfold benchmark");
        assert_eq!(super::from_wide(&field), "Pinfold");
        assert_eq!(field[7], 0);
    }

    #[test]
    fn empty_and_shell_only() {
        assert!(assign_order(Vec::new(), Some(SHELL), 30).is_empty());
        assert_eq!(assign_order(z(&[(SHELL, 1)]), Some(SHELL), 30), [SHELL]);
    }
}
