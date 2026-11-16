//! The benchmark from the tray menu. It runs on a separate thread in this
//! process. Thus it has the elevation of this instance (PresentMon needs
//! it), and the engine on the main thread applies each profile switch.

use super::*;

/// The benchmark state that the benchmark thread and the tray share.
pub(super) struct BenchState {
    /// Set while a benchmark runs. Store true to stop it.
    pub(super) cancel: Option<Arc<AtomicBool>>,
    /// Progress text for the tooltip.
    pub(super) status: Option<String>,
    /// Part of the captures that is done, 0 to 1.
    pub(super) progress: f64,
    /// A notification for the main thread (title, text).
    pub(super) note: Option<(String, String)>,
}

pub(super) static BENCH: Mutex<BenchState> =
    Mutex::new(BenchState { cancel: None, status: None, progress: 0.0, note: None });
pub(super) const WM_BENCH_STATUS: u32 = WM_APP + 2;
pub(super) const WM_BENCH_NOTE: u32 = WM_APP + 3;

/// Lock the benchmark state. Keep the lock only for a short time.
pub(super) fn bench_state() -> MutexGuard<'static, BenchState> {
    BENCH.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn bench_running() -> bool {
    bench_state().cancel.is_some()
}

/// Comparisons for a rule: Cache and Frequency, the profile of the rule and
/// each other profile, and all profiles.
pub(super) fn benchmark_choices(profiles: &[String], current: &str) -> Vec<Vec<String>> {
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

pub(super) fn choice_label(choice: &[String], all: usize) -> String {
    if choice.len() > 2 && choice.len() == all {
        format!("All {all} profiles")
    } else {
        choice.join(" vs ")
    }
}

/// The report of the last benchmark.
pub(super) fn last_report() -> Option<std::path::PathBuf> {
    let dir = config::dir().join("benchmarks");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path().join("report.txt"))
        .filter_map(|p| std::fs::metadata(&p).ok()?.modified().ok().map(|t| (t, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

pub(super) struct TrayUi {
    hwnd: usize,
}

impl bench::Ui for TrayUi {
    fn say(&self, line: &str) {
        crate::log::log(&format!("benchmark: {line}"));
    }
    fn status(&self, text: &str) {
        bench_state().status = Some(text.to_string());
        unsafe { PostMessageW(self.hwnd as HWND, WM_BENCH_STATUS, 0, 0) };
    }
    fn sound(&self, sound: bench::Sound) {
        bench::play(sound);
    }
    fn progress(&self, done: f64) {
        bench_state().progress = done.clamp(0.0, 1.0);
    }
}

pub(super) fn notify(hwnd: usize, title: &str, text: &str) {
    bench_state().note = Some((title.to_string(), text.to_string()));
    unsafe { PostMessageW(hwnd as HWND, WM_BENCH_NOTE, 0, 0) };
}

pub(super) const BENCH_TITLE: &str = "Pinfold benchmark";

pub(super) fn ask(text: &str) -> bool {
    dialog::ask(BENCH_TITLE, text)
}

pub(super) fn tell(text: &str) {
    dialog::tell(BENCH_TITLE, text)
}
pub(super) const BUTTON_APPLY: i32 = 100;
pub(super) const BUTTON_REPORT: i32 = 101;
pub(super) const BUTTON_CLOSE: i32 = 102;

/// The benchmark result. One button opens the report and the dialog stays
/// open. If another profile won, one button makes it the rule. Returns true
/// if the winner must become the rule.
pub(super) fn result_dialog(
    heading: &str,
    text: &str,
    report: &str,
    apply: Option<(&str, &str)>,
) -> bool {
    let buttons = match apply {
        Some((winner, current)) => vec![
            (BUTTON_APPLY, format!("Make {winner} the rule")),
            (BUTTON_REPORT, "Open report".to_string()),
            (BUTTON_CLOSE, format!("Keep {current}")),
        ],
        None => {
            vec![(BUTTON_REPORT, "Open report".to_string()), (BUTTON_CLOSE, "Close".to_string())]
        }
    };
    let open_report = |id: i32| {
        if id == BUTTON_REPORT {
            open(report);
        }
        id == BUTTON_REPORT
    };
    let dialog = TaskDialog {
        title: BENCH_TITLE,
        heading,
        text,
        buttons,
        on_button: Some(&open_report),
        ..Default::default()
    };
    match dialog.show() {
        Some(pressed) => pressed == BUTTON_APPLY,
        // No task dialog: use a message box.
        None => {
            let plain = format!("{heading}\n\n{text}\n\nReport: {report}");
            match apply {
                Some((winner, _)) => ask(&format!("{plain}\n\nMake {winner} the rule?")),
                None => {
                    tell(&plain);
                    false
                }
            }
        }
    }
}
pub(super) fn start_benchmark(hwnd: HWND, target: String, profiles: Vec<String>) {
    let paused = ENGINE.with(|e| e.borrow().as_ref().map(|e| e.paused)).unwrap_or(false);
    if paused {
        message_box("Pinfold is paused, so it cannot switch profiles. Resume it first.");
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut state = bench_state();
        if state.cancel.is_some() {
            return;
        }
        state.cancel = Some(cancel.clone());
        state.progress = 0.0;
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
        {
            let mut state = bench_state();
            state.cancel = None;
            state.status = None;
        }
        unsafe { PostMessageW(hwnd as HWND, WM_BENCH_STATUS, 0, 0) };
        finish_benchmark(hwnd, &target, result);
    });
}

/// Show the result. Runs on the benchmark thread, thus the dialogs do not
/// block the tray.
pub(super) fn finish_benchmark(
    hwnd: usize,
    target: &str,
    result: Result<bench::Outcome, bench::Failure>,
) {
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

pub(super) fn stop_benchmark() {
    if let Some(cancel) = &bench_state().cancel {
        cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
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
}
