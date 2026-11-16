//! The tray menu: build it when it opens, and do the selected action.

use super::*;

pub(super) enum Action {
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
    About,
    Exit,
    Nothing,
}

pub(super) struct Menu {
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
pub(super) unsafe fn track(hwnd: HWND, hmenu: HMENU, mut m: Menu) -> Action {
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

pub(super) unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
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
pub(super) fn assign_order(
    z_order: Vec<(String, u64)>,
    shell: Option<&str>,
    limit: usize,
) -> Vec<String> {
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
pub(super) fn windowed_processes() -> Vec<String> {
    let mut pids: Vec<u32> = Vec::new();
    unsafe { EnumWindows(Some(enum_windows_cb), &mut pids as *mut Vec<u32> as LPARAM) };
    let z_order: Vec<(String, u64)> = pids.into_iter().filter_map(process_info).collect();
    let mut shell_pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetShellWindow(), &mut shell_pid) };
    let shell = process_info(shell_pid).map(|(path, _)| path);
    assign_order(z_order, shell.as_deref(), 30)
}
pub(super) fn build_menu(e: &Engine) -> (HMENU, Menu) {
    let mut m = Menu { actions: Vec::new() };
    let running = bench_running();
    unsafe {
        let root = CreatePopupMenu();
        if running {
            let status = bench_state().status.clone().unwrap_or_default();
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
        m.item(root, "About Pinfold", false, Action::About);
        m.item(root, "Exit", false, Action::Exit);
        (root, m)
    }
}

pub(super) unsafe fn show_menu(hwnd: HWND) {
    // Do not keep the borrow while the menu is open: the poll timer
    // continues in the modal loop of the menu.
    let built = ENGINE.with(|e| e.borrow().as_ref().map(build_menu));
    let Some((hmenu, menu)) = built else { return };
    let action = track(hwnd, hmenu, menu);
    handle(hwnd, action);
}

pub(super) fn with_engine(f: impl FnOnce(&mut Engine)) {
    ENGINE.with(|e| {
        if let Ok(mut guard) = e.try_borrow_mut() {
            if let Some(engine) = guard.as_mut() {
                f(engine);
            }
        }
    });
}

/// Change one rule from the menu, then save and apply.
pub(super) fn edit_rule(pattern: &str, change: impl FnOnce(&mut config::Rule)) {
    with_engine(|e| {
        if let Some(rule) = e.cfg.rule_mut(pattern) {
            change(rule);
            e.save_and_apply();
        }
    });
}

pub(super) unsafe fn handle(hwnd: HWND, action: Action) {
    match action {
        Action::Nothing => {}
        Action::Assign { path, profile } => with_engine(|e| {
            e.cfg.upsert_rule(file_name(&path), &profile, None);
            e.save_and_apply();
        }),
        Action::SetProfile { pattern, profile } => edit_rule(&pattern, |r| r.profile = profile),
        Action::SetMode { pattern, mode } => edit_rule(&pattern, |r| r.mode = mode),
        Action::ToggleEnforce { pattern } => edit_rule(&pattern, |r| r.enforce = !r.enforce),
        Action::Remove { pattern } => with_engine(|e| {
            if e.cfg.remove_rule(&pattern).is_some() {
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
        Action::About => about(),
        Action::Exit => {
            DestroyWindow(hwnd);
        }
    }
}

pub(super) unsafe fn add_program(hwnd: HWND) {
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

pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(super) const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Name, version, license and a link to the GitHub page.
pub(super) fn about() {
    let heading = format!("Pinfold {VERSION}");
    let text = format!(
        "Keeps programs on the CPU cores that you select.\n\nMIT license\n<a href=\"{REPOSITORY}\">{}</a>",
        REPOSITORY.trim_start_matches("https://")
    );
    let dialog = TaskDialog {
        title: "About Pinfold",
        heading: &heading,
        text: &text,
        links: true,
        on_link: Some(&|link: &str| open(link)),
        ..Default::default()
    };
    if dialog.show().is_none() {
        message_box(&format!("Pinfold {VERSION}\n\nMIT license\n{REPOSITORY}"));
    }
}

// --- Start with Windows -----------------------------------------------------

pub(super) unsafe fn toggle_autostart(hwnd: HWND) {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn empty_and_shell_only() {
        assert!(assign_order(Vec::new(), Some(SHELL), 30).is_empty());
        assert_eq!(assign_order(z(&[(SHELL, 1)]), Some(SHELL), 30), [SHELL]);
    }
}
