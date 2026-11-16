//! `Pinfold.exe benchmark`: moves a running program from profile to
//! profile, records frame times with PresentMon and compares them.
//!
//! The switch changes the rule in the config. Thus a running background
//! instance applies it (and does not see it as drift). If there is no
//! background instance, the benchmark applies the affinity. At the end,
//! the rule is as before.

use std::cell::Cell;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;

use crate::config::{self, Config, Mode, Rule};
use crate::engine::System;
use crate::frametime::{self, Stats};
use crate::sys::{affinity_of, pids_by_name, RealSystem};
use crate::topology::{self, format_mask, Topology};
use crate::win::file_name;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const PRESENTMON_HELP: &str = "PresentMon was not found.\n\
     Download the PresentMon console application from\n\
     https://github.com/GameTechDev/PresentMon/releases\n\
     and either put the .exe next to Pinfold.exe, set \"presentMon\" in config.json\n\
     to its path, or pass --presentmon <path>.";

#[derive(Debug, PartialEq)]
pub struct Options {
    pub target: String,
    pub profiles: Vec<String>,
    pub seconds: u32,
    pub rounds: u32,
    pub settle: u32,
    pub countdown: u32,
    pub presentmon: Option<String>,
    /// Capture only when the target is the window in front.
    pub focus: bool,
    /// Make the winner the rule without asking.
    pub apply: bool,
    /// Started from the tray menu: new console window, wait before it closes.
    pub window: bool,
}

impl Options {
    /// The command line for these options.
    pub fn to_args(&self) -> Vec<String> {
        let mut a = vec![self.target.clone()];
        if !self.profiles.is_empty() {
            a.extend(["--profiles".to_string(), self.profiles.join(",")]);
        }
        for (flag, value) in [
            ("--seconds", self.seconds),
            ("--rounds", self.rounds),
            ("--settle", self.settle),
            ("--countdown", self.countdown),
        ] {
            a.extend([flag.to_string(), value.to_string()]);
        }
        if let Some(p) = &self.presentmon {
            a.extend(["--presentmon".to_string(), p.clone()]);
        }
        if !self.focus {
            a.push("--ignore-focus".to_string());
        }
        if self.apply {
            a.push("--apply".to_string());
        }
        if self.window {
            a.push("--window".to_string());
        }
        a
    }
}

pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        target: String::new(),
        profiles: Vec::new(),
        seconds: 60,
        rounds: 2,
        settle: 5,
        countdown: 3,
        presentmon: None,
        focus: true,
        apply: false,
        window: false,
    };
    let number = |flag: &str, v: Option<&String>, min: u32, max: u32| -> Result<u32, String> {
        v.and_then(|v| v.parse::<u32>().ok())
            .filter(|n| (min..=max).contains(n))
            .ok_or(format!("{flag} needs a number from {min} to {max}"))
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profiles" => {
                let list = it.next().ok_or("--profiles needs a comma-separated list")?;
                o.profiles = list
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
            }
            "--seconds" => o.seconds = number("--seconds", it.next(), 1, 3600)?,
            "--rounds" => o.rounds = number("--rounds", it.next(), 1, 20)?,
            "--settle" => o.settle = number("--settle", it.next(), 0, 300)?,
            "--countdown" => o.countdown = number("--countdown", it.next(), 0, 600)?,
            "--presentmon" => {
                o.presentmon = Some(it.next().ok_or("--presentmon needs a path")?.clone())
            }
            "--apply" => o.apply = true,
            "--ignore-focus" => o.focus = false,
            "--window" => o.window = true,
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ if o.target.is_empty() => o.target = file_name(a).to_string(),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    if o.target.is_empty() {
        return Err("which program? Give the file name of a running program, e.g. game.exe".into());
    }
    Ok(o)
}

/// Capture order: each profile one time in each round. Each second round
/// uses the reverse order. Thus warm-up and slow scene changes do not
/// help the profile that comes first.
pub fn phases(profiles: &[String], rounds: u32) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for round in 1..=rounds {
        let mut order: Vec<String> = profiles.to_vec();
        if round % 2 == 0 {
            order.reverse();
        }
        out.extend(order.into_iter().map(|p| (round, p)));
    }
    out
}

/// "Cache" and "Frequency" if both exist, else all profiles.
pub fn default_profiles(cfg: &Config) -> Vec<String> {
    match (cfg.profile("Cache"), cfg.profile("Frequency")) {
        (Some(c), Some(f)) => vec![c.name.clone(), f.name.clone()],
        _ => cfg.profiles.iter().map(|p| p.name.clone()).collect(),
    }
}

fn is_presentmon(name: &str) -> bool {
    let n = name.to_lowercase();
    n.starts_with("presentmon") && (n.ends_with(".exe") || n.ends_with(".cmd"))
}

/// First the given path. Then a PresentMon*.exe next to Pinfold.exe or in
/// the config directory (release files have a version in the name). Then
/// PresentMon.exe on PATH.
pub fn find_presentmon(explicit: Option<&str>, search: &[PathBuf]) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        let path = PathBuf::from(p);
        return if path.is_file() { Ok(path) } else { Err(format!("PresentMon not found at {p}")) };
    }
    for dir in search {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file() && p.file_name().is_some_and(|n| is_presentmon(&n.to_string_lossy()))
            })
            .collect();
        found.sort();
        if let Some(p) = found.pop() {
            return Ok(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join("PresentMon.exe");
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(PRESENTMON_HELP.to_string())
}

/// How the benchmark tells the user about its progress: console or tray.
pub trait Ui {
    /// A line of progress (console output or the log).
    fn say(&self, line: &str);
    /// Short state for a title bar or a tooltip.
    fn status(&self, text: &str);
    fn sound(&self, sound: Sound);
    /// Share of the captures done, 0 to 1.
    fn progress(&self, _done: f64) {}
}

/// During the benchmark, the player is in the game. Sounds are the
/// signals that the player gets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sound {
    /// The first capture starts.
    Start,
    /// The capture of the next profile starts.
    Switch,
    /// A problem (focus lost, failure).
    Problem,
    Done,
}

pub fn play(sound: Sound) {
    // MB_OK, MB_ICONEXCLAMATION, MB_ICONHAND, MB_ICONASTERISK: four sounds
    // of the Windows sound scheme.
    let kind = match sound {
        Sound::Switch => 0,
        Sound::Start => 0x30,
        Sound::Problem => 0x10,
        Sound::Done => 0x40,
    };
    unsafe { MessageBeep(kind) };
}

/// Why a capture stopped before PresentMon finished.
#[derive(Debug, PartialEq)]
enum Interrupt {
    Cancelled,
    FocusLost,
}

struct Run<'a> {
    opts: &'a Options,
    ui: &'a dyn Ui,
    cancel: &'a AtomicBool,
    dir: PathBuf,
    cfg: Config,
    topo: Topology,
    presentmon: PathBuf,
    out_dir: PathBuf,
    /// If true, a background instance applies config changes. If false, we do.
    background: bool,
    /// Number of captures in the plan.
    steps: usize,
}

/// Process id of the window in front.
fn foreground_pid() -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        if !hwnd.is_null() {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        pid
    }
}

impl Run<'_> {
    fn cancelled(&self) -> Result<(), Failure> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Failure::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Wait with a countdown in the status. A cancel stops the wait.
    fn pause(&self, secs: u32, status: &str) -> Result<(), Failure> {
        for left in (1..=secs).rev() {
            self.ui.status(&format!("{status} in {left} s"));
            for _ in 0..10 {
                self.cancelled()?;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.cancelled()
    }

    fn in_front(&self) -> bool {
        pids_by_name(&self.opts.target).contains(&foreground_pid())
    }

    /// A capture counts only when the game is the window in front.
    fn wait_for_focus(&self) -> Result<(), Failure> {
        if !self.opts.focus || self.in_front() {
            return Ok(());
        }
        let target = &self.opts.target;
        self.ui.say(&format!("Waiting for {target} to be in front - switch to it."));
        self.ui.status(&format!("waiting for {target} to be in front"));
        loop {
            self.cancelled()?;
            if pids_by_name(target).is_empty() {
                return Err(format!("{target} is no longer running").into());
            }
            if self.in_front() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn mask_of(&self, profile: &str) -> Result<u64, String> {
        let p = self.cfg.profile(profile).ok_or(format!("unknown profile '{profile}'"))?;
        let mask = topology::parse_cpus(&p.cpus)? & self.topo.all_mask;
        if mask == 0 {
            return Err(format!("profile '{}' has no cpus on this system", p.name));
        }
        Ok(mask)
    }

    fn mode(&self) -> Mode {
        self.cfg.rule_index(&self.opts.target).map(|i| self.cfg.rules[i].mode).unwrap_or_default()
    }

    /// Set the rule to `profile` and wait until the running program has it.
    fn switch(&mut self, profile: &str) -> Result<(), String> {
        let mask = self.mask_of(profile)?;
        self.cfg.upsert_rule(&self.opts.target, profile, None);
        config::save(&self.dir, &self.cfg).map_err(|e| format!("cannot write config: {e}"))?;
        let mode = self.mode();
        if !self.background {
            self.apply_directly(mask, mode);
        }
        if mode == Mode::Cpusets {
            // To read CPU Sets, the process must be opened with more than
            // query rights. Wait for one poll of the engine.
            std::thread::sleep(Duration::from_millis(self.cfg.poll_ms() as u64 + 500));
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_millis(self.cfg.poll_ms() as u64 * 3 + 3000);
        loop {
            let pids = pids_by_name(&self.opts.target);
            if pids.is_empty() {
                return Err(format!("{} is no longer running", self.opts.target));
            }
            if pids.iter().all(|&pid| affinity_of(pid).is_none_or(|a| a == mask)) {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "{} did not take the profile '{profile}'. If the program runs elevated, \
                     Pinfold has to run elevated too (Start with Windows does that).",
                    self.opts.target
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn apply_directly(&self, mask: u64, mode: Mode) {
        let mut sys = RealSystem;
        for pid in pids_by_name(&self.opts.target) {
            if let Ok(h) = sys.open(pid) {
                if mode != Mode::Cpusets {
                    let _ = sys.set_affinity(h, mask);
                }
                if mode != Mode::Affinity {
                    let _ = sys.set_cpu_sets(h, &self.topo.cpu_set_ids(mask));
                }
                sys.close(h);
            }
        }
    }

    /// Capture `profile` (already applied) when the game is in front. If
    /// the game loses the focus for more than one second, discard the
    /// capture and do it again.
    fn capture(
        &self,
        index: usize,
        round: u32,
        profile: &str,
        label: &str,
    ) -> Result<frametime::Capture, Failure> {
        let target = &self.opts.target;
        let safe: String =
            profile.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let csv = self.out_dir.join(format!("round{round}-{safe}.csv"));
        let steps = self.steps.max(1) as f64;
        loop {
            self.ui.progress(index as f64 / steps);
            self.wait_for_focus()?;
            self.pause(self.opts.settle, &format!("{label}: capture starts"))?;
            let game = pids_by_name(target);
            let started = Instant::now();
            let lost_since: Cell<Option<Instant>> = Cell::new(None);
            let shown = Cell::new(u64::MAX);
            let watch = || {
                if self.cancel.load(Ordering::Relaxed) {
                    return Some(Interrupt::Cancelled);
                }
                let left = (self.opts.seconds as u64).saturating_sub(started.elapsed().as_secs());
                if shown.replace(left) != left {
                    self.ui.status(&format!("{label}: {left} s left"));
                    let part = started.elapsed().as_secs_f64() / self.opts.seconds.max(1) as f64;
                    self.ui.progress((index as f64 + part.min(1.0)) / steps);
                }
                if self.opts.focus && !game.contains(&foreground_pid()) {
                    let since = lost_since.get().unwrap_or_else(Instant::now);
                    lost_since.set(Some(since));
                    if since.elapsed() > Duration::from_secs(1) {
                        return Some(Interrupt::FocusLost);
                    }
                } else {
                    lost_since.set(None);
                }
                None
            };
            let _ = std::fs::remove_file(&csv);
            match presentmon_capture(&self.presentmon, &csv, self.opts.seconds, &watch) {
                Ok(()) => break,
                Err(CaptureError::Interrupted(Interrupt::Cancelled)) => {
                    return Err(Failure::Cancelled)
                }
                Err(CaptureError::Interrupted(Interrupt::FocusLost)) => {
                    self.ui.sound(Sound::Problem);
                    self.ui.say(&format!(
                        "{target} was not in front: this capture is repeated once it is back."
                    ));
                }
                Err(e) => return Err(e.message().into()),
            }
        }
        let text = std::fs::read_to_string(&csv)
            .map_err(|_| format!("PresentMon wrote no capture file ({})", csv.display()))?;
        let capture = frametime::parse_csv(&text, target).map_err(|e| {
            format!("{e} ({}). Was {target} in front and rendering?", csv.display())
        })?;
        Ok(capture)
    }
}

/// Why PresentMon did not make a capture.
enum CaptureError {
    Failed { code: Option<i32>, output: String },
    Interrupted(Interrupt),
}

impl CaptureError {
    fn failed(output: impl Into<String>) -> CaptureError {
        CaptureError::Failed { code: None, output: output.into() }
    }

    /// PresentMon could not start its trace session: no rights.
    fn is_access_denied(&self) -> bool {
        matches!(self, CaptureError::Failed { output, .. }
            if output.to_lowercase().contains("access denied"))
    }

    fn message(&self) -> String {
        match self {
            CaptureError::Interrupted(Interrupt::Cancelled) => "cancelled".into(),
            CaptureError::Interrupted(Interrupt::FocusLost) => "the game lost focus".into(),
            CaptureError::Failed { code, output } => {
                let said = output.trim();
                let code = code.map(|c| c.to_string()).unwrap_or_else(|| "none".into());
                if said.is_empty() {
                    format!("PresentMon failed (exit code {code}) without a message.")
                } else {
                    format!("PresentMon failed (exit code {code}):\n{said}")
                }
            }
        }
    }
}

/// One timed PresentMon capture into `csv`. The capture has all programs.
/// The target is selected after the capture. Thus a capture without the
/// target shows the programs with frames, and not only an empty file.
/// `watch` is called ten times each second. It can stop the capture.
fn presentmon_capture(
    presentmon: &Path,
    csv: &Path,
    seconds: u32,
    watch: &dyn Fn() -> Option<Interrupt>,
) -> Result<(), CaptureError> {
    // PresentMon writes problems to stderr. Keep it for the error message.
    let err_path = csv.with_extension("presentmon.txt");
    let err_file =
        std::fs::File::create(&err_path).map_err(|e| CaptureError::failed(e.to_string()))?;
    let mut child = Command::new(presentmon)
        .arg("--output_file")
        .arg(csv)
        .args(["--timed", &seconds.to_string()])
        .args(["--terminate_after_timed", "--no_console_stats"])
        .args(["--session_name", "PinfoldBenchmark", "--stop_existing_session"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| CaptureError::failed(format!("cannot start {}: {e}", presentmon.display())))?;
    let deadline = Instant::now() + Duration::from_secs(seconds as u64 + 30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return Err(CaptureError::failed("PresentMon did not finish in time"));
            }
            Ok(None) => {
                if let Some(why) = watch() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&err_path);
                    let _ = std::fs::remove_file(csv);
                    return Err(CaptureError::Interrupted(why));
                }
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(e) => {
                return Err(CaptureError::failed(format!("waiting for PresentMon failed: {e}")))
            }
        }
    };
    let output = std::fs::read_to_string(&err_path).unwrap_or_default();
    if status.success() {
        let _ = std::fs::remove_file(&err_path);
        Ok(())
    } else {
        Err(CaptureError::Failed { code: status.code(), output })
    }
}

/// Why the benchmark did not make a report.
pub enum Failure {
    /// PresentMon needs rights that this process does not have. Run the
    /// benchmark elevated.
    NeedsElevation,
    /// Stopped by the user. The rule is as before.
    Cancelled,
    Message(String),
}

impl From<String> for Failure {
    fn from(s: String) -> Failure {
        Failure::Message(s)
    }
}

impl From<&str> for Failure {
    fn from(s: &str) -> Failure {
        Failure::Message(s.to_string())
    }
}

fn restore(dir: &Path, target: &str, original: Option<Rule>, reset: Option<u64>) {
    // Read again: change only our rule, keep all other changes.
    if let Ok(mut cfg) = config::load(dir) {
        if let Some(i) = cfg.rule_index(target) {
            match original {
                Some(rule) => cfg.rules[i] = rule,
                None => {
                    cfg.rules.remove(i);
                }
            }
            let _ = config::save(dir, &cfg);
        }
    }
    // If there was no rule and no engine, nothing else resets the program.
    if let Some(all) = reset {
        let mut sys = RealSystem;
        for pid in pids_by_name(target) {
            if let Ok(h) = sys.open(pid) {
                let _ = sys.set_affinity(h, all);
                let _ = sys.set_cpu_sets(h, &[]);
                sys.close(h);
            }
        }
    }
}

/// The profiles to compare, after a check against the config.
pub fn chosen_profiles(opts: &Options, cfg: &Config) -> Result<Vec<String>, String> {
    let profiles: Vec<String> = if opts.profiles.is_empty() {
        default_profiles(cfg)
    } else {
        opts.profiles
            .iter()
            .map(|p| cfg.profile(p).map(|p| p.name.clone()).ok_or(format!("unknown profile '{p}'")))
            .collect::<Result<_, _>>()?
    };
    if profiles.len() < 2 {
        return Err("a comparison needs at least two profiles".into());
    }
    Ok(profiles)
}

/// Run the benchmark. `cancel` stops it at the next check (ten times each
/// second). The rule is always put back.
pub fn run(opts: &Options, ui: &dyn Ui, cancel: &AtomicBool) -> Result<Outcome, Failure> {
    let dir = config::dir();
    let cfg = config::load(&dir)?;
    let topo = topology::read();
    let profiles = chosen_profiles(opts, &cfg)?;
    let configured = opts.presentmon.clone().or(cfg.present_mon.clone());
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let search: Vec<PathBuf> = exe_dir.into_iter().chain([dir.clone()]).collect();
    let presentmon = find_presentmon(configured.as_deref(), &search)?;

    if pids_by_name(&opts.target).is_empty() {
        return Err(format!("{} is not running. Start it first.", opts.target).into());
    }
    let out_dir = dir.join("benchmarks").join(format!(
        "{}-{}",
        opts.target.trim_end_matches(".exe"),
        crate::log::timestamp_compact()
    ));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;

    // One-second test capture before the user goes to the game. It finds
    // missing rights and other PresentMon problems early.
    ui.status("checking PresentMon");
    let trial = out_dir.join("trial.csv");
    let checked = presentmon_capture(&presentmon, &trial, 1, &|| {
        cancel.load(Ordering::Relaxed).then_some(Interrupt::Cancelled)
    });
    let _ = std::fs::remove_file(&trial);
    if let Err(e) = checked {
        let _ = std::fs::remove_dir_all(&out_dir);
        return Err(match e {
            CaptureError::Interrupted(Interrupt::Cancelled) => Failure::Cancelled,
            e if e.is_access_denied() && !crate::diag::is_elevated() => Failure::NeedsElevation,
            e => Failure::Message(e.message()),
        });
    }

    let original = cfg.rule_index(&opts.target).map(|i| cfg.rules[i].clone());
    let background = crate::tray::is_running();
    let plan = phases(&profiles, opts.rounds);
    let total = opts.countdown + plan.len() as u32 * (opts.settle + opts.seconds);
    ui.say(&format!("Benchmark of {}: {}", opts.target, profiles.join(" vs ")));
    ui.say(&format!(
        "{} captures of {} s, about {} min {} s in total. PresentMon: {}",
        plan.len(),
        opts.seconds,
        total / 60,
        total % 60,
        presentmon.display()
    ));
    ui.say("Keep the game in front and the scene as constant as you can.");
    ui.say("A sound marks the start, every switch and the end.");

    let mut run = Run {
        opts,
        ui,
        cancel,
        dir: dir.clone(),
        cfg,
        topo,
        presentmon,
        out_dir,
        background,
        steps: plan.len(),
    };
    let mut frames: Vec<(String, Vec<f64>)> =
        profiles.iter().map(|p| (p.clone(), Vec::new())).collect();
    let mut rounds_text = String::new();
    let mut per_round: Vec<(u32, String, f64)> = Vec::new();
    // GPU busy time and total frame time for each profile. None if a
    // capture has no GPU column.
    let mut gpu: Vec<(String, Option<(f64, f64)>)> =
        profiles.iter().map(|p| (p.clone(), Some((0.0, 0.0)))).collect();
    let mut outcome = Ok(());
    let started = (|| {
        run.wait_for_focus()?;
        run.pause(opts.countdown, "benchmark starts")
    })();
    if let Err(e) = started {
        outcome = Err(e);
    }
    for (i, (round, profile)) in plan.iter().enumerate() {
        if outcome.is_err() {
            break;
        }
        ui.sound(if i == 0 { Sound::Start } else { Sound::Switch });
        let label = format!("capture {}/{}, {profile}", i + 1, plan.len());
        let step = (|| {
            run.switch(profile)?;
            let mask = run.mask_of(profile)?;
            ui.say(&format!("round {round}: {profile} (cpus {}) ...", format_mask(mask)));
            let capture = run.capture(i, *round, profile, &label)?;
            let times = capture.times;
            if let Some((_, sums)) = gpu.iter_mut().find(|(p, _)| p == profile) {
                let total: f64 = times.iter().sum();
                *sums = match (*sums, capture.gpu_share) {
                    (Some((b, t)), Some(share)) => Some((b + share * total, t + total)),
                    _ => None,
                };
            }
            let s = frametime::stats(&times).ok_or("the capture contains no frames")?;
            let line = format!(
                "round {round}: {profile:<12} {:>8.1} avg FPS  {:>8.1} 1% low  {:>7.2} ms p99  ({} frames)",
                s.avg_fps, s.low1_fps, s.p99_ms, s.frames
            );
            ui.say(&line);
            rounds_text.push_str(&line);
            rounds_text.push('\n');
            per_round.push((*round, profile.clone(), s.avg_fps));
            if let Some((_, all)) = frames.iter_mut().find(|(p, _)| p == profile) {
                all.extend(times);
            }
            Ok::<(), Failure>(())
        })();
        if let Err(e) = step {
            outcome = Err(e);
        }
    }
    let current = original.as_ref().map(|r| r.profile.clone());
    let reset = (!background && original.is_none()).then_some(run.topo.all_mask);
    restore(&dir, &opts.target, original, reset);
    if let Err(e) = outcome {
        if !matches!(e, Failure::Cancelled) {
            ui.sound(Sound::Problem);
        }
        return Err(e);
    }
    ui.sound(Sound::Done);

    let columns: Vec<(String, Stats)> = frames
        .iter()
        .filter_map(|(p, times)| frametime::stats(times).map(|s| (p.clone(), s)))
        .collect();
    let overall: Vec<(String, f64)> = columns.iter().map(|(p, s)| (p.clone(), s.avg_fps)).collect();
    let verdict = frametime::verdict(&overall, &per_round);
    let shares: Option<Vec<(String, f64)>> = gpu
        .iter()
        .map(|(p, sums)| sums.filter(|(_, t)| *t > 0.0).map(|(b, t)| (p.clone(), b / t)))
        .collect();
    let gpu_note = shares.and_then(|s| frametime::gpu_note(&s));
    let mut text = format!(
        "Pinfold benchmark of {}\n{} rounds, {} s per capture, all rounds of a profile combined.\n\n\
         Verdict: {}\n",
        opts.target, opts.rounds, opts.seconds, verdict.text
    );
    if let Some(note) = &gpu_note {
        text.push_str(note);
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&frametime::report(&columns));
    text.push_str(
        "\nFPS rows: higher is better. Frame time rows: lower is better.\n\
         Differences under about 2-3 % are within run-to-run noise.\n\nPer round:\n",
    );
    text.push_str(&rounds_text);
    let report_path = run.out_dir.join("report.txt");
    let _ = std::fs::write(&report_path, &text);
    text.push_str(&format!("\nCaptures and this report: {}\n", run.out_dir.display()));
    let summary = columns
        .iter()
        .map(|(p, s)| {
            format!(
                "{p}: {:.1} avg FPS, {:.1} 1% low, {:.1} ms p99",
                s.avg_fps, s.low1_fps, s.p99_ms
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome { report: text, verdict, summary, gpu_note, current, report_path })
}

/// A finished benchmark.
pub struct Outcome {
    /// The full report, as printed and saved.
    pub report: String,
    pub verdict: frametime::Verdict,
    /// One line for each profile with the main numbers.
    pub summary: String,
    /// The GPU load, and if it hid the effect of the CPU placement.
    pub gpu_note: Option<String>,
    /// The profile of the rule before the benchmark, if there was a rule.
    pub current: Option<String>,
    pub report_path: PathBuf,
}

impl Outcome {
    /// The winner, if it is not the profile of the rule.
    pub fn better_profile(&self) -> Option<&str> {
        let w = self.verdict.winner.as_deref()?;
        match &self.current {
            Some(c) if c.eq_ignore_ascii_case(w) => None,
            _ => Some(w),
        }
    }
}

/// Make `profile` the rule for `target` (the result of the benchmark).
pub fn apply(target: &str, profile: &str) -> Result<(), String> {
    let dir = config::dir();
    let mut cfg = config::load(&dir)?;
    cfg.upsert_rule(target, profile, None);
    config::save(&dir, &cfg).map_err(|e| format!("cannot write config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults() {
        let o = parse_args(&args(&["D:\\Games\\Game.exe"])).unwrap();
        assert_eq!(o.target, "Game.exe");
        assert_eq!((o.seconds, o.rounds, o.settle, o.countdown), (60, 2, 5, 3));
        assert!(o.profiles.is_empty() && o.presentmon.is_none() && !o.window);
        assert!(o.focus && !o.apply);
    }

    #[test]
    fn all_options() {
        let o = parse_args(&args(&[
            "game.exe",
            "--profiles",
            "Cache, Frequency,All cores",
            "--seconds",
            "90",
            "--rounds",
            "3",
            "--settle",
            "0",
            "--countdown",
            "0",
            "--presentmon",
            "C:\\pm.exe",
            "--window",
        ]))
        .unwrap();
        assert_eq!(o.profiles, ["Cache", "Frequency", "All cores"]);
        assert_eq!((o.seconds, o.rounds, o.settle, o.countdown), (90, 3, 0, 0));
        assert_eq!(o.presentmon.as_deref(), Some("C:\\pm.exe"));
        assert!(o.window);
    }

    #[test]
    fn options_survive_a_round_trip_through_the_command_line() {
        let o = parse_args(&args(&[
            "game.exe",
            "--profiles",
            "Cache,All cores",
            "--seconds",
            "90",
            "--rounds",
            "3",
            "--settle",
            "1",
            "--countdown",
            "2",
            "--presentmon",
            "C:\\my tools\\pm.exe",
            "--ignore-focus",
            "--apply",
            "--window",
        ]))
        .unwrap();
        assert_eq!(parse_args(&o.to_args()).unwrap(), o);
        let plain = parse_args(&args(&["game.exe"])).unwrap();
        assert_eq!(parse_args(&plain.to_args()).unwrap(), plain);
    }

    #[test]
    fn access_denied_is_recognised_from_what_presentmon_says() {
        let denied = CaptureError::Failed {
            code: Some(6),
            output: "error: failed to start trace session: access denied.\n".into(),
        };
        assert!(denied.is_access_denied());
        assert!(denied.message().contains("exit code 6"));
        assert!(denied.message().contains("failed to start trace session"));
        let other = CaptureError::Failed { code: Some(5), output: String::new() };
        assert!(!CaptureError::Interrupted(Interrupt::FocusLost).is_access_denied());
        assert!(!other.is_access_denied());
        assert!(other.message().contains("without a message"));
    }

    #[test]
    fn bad_arguments_are_explained() {
        assert!(parse_args(&args(&[])).unwrap_err().contains("which program"));
        assert!(parse_args(&args(&["g.exe", "--seconds", "0"])).unwrap_err().contains("--seconds"));
        assert!(parse_args(&args(&["g.exe", "--rounds", "x"])).unwrap_err().contains("--rounds"));
        assert!(parse_args(&args(&["g.exe", "--nope"])).unwrap_err().contains("unknown option"));
        assert!(parse_args(&args(&["g.exe", "h.exe"])).unwrap_err().contains("unexpected"));
    }

    #[test]
    fn order_is_reversed_every_second_round() {
        let p = vec!["A".to_string(), "B".to_string()];
        let order: Vec<String> =
            phases(&p, 3).into_iter().map(|(r, p)| format!("{r}{p}")).collect();
        assert_eq!(order, ["1A", "1B", "2B", "2A", "3A", "3B"]);
    }

    fn cfg(names: &[&str]) -> Config {
        Config {
            poll_interval_ms: 1000,
            elevated_autostart: true,
            present_mon: None,
            profiles: names
                .iter()
                .map(|n| Profile { name: n.to_string(), cpus: "0".into() })
                .collect(),
            rules: vec![],
        }
    }

    #[test]
    fn default_comparison_is_cache_against_frequency() {
        assert_eq!(
            default_profiles(&cfg(&["Cache", "Frequency", "All cores"])),
            ["Cache", "Frequency"]
        );
        assert_eq!(default_profiles(&cfg(&["All cores", "Half"])), ["All cores", "Half"]);
    }

    #[test]
    fn presentmon_is_found_next_to_the_exe_with_a_versioned_name() {
        let dir = std::env::temp_dir().join(format!("pinfold-pm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.txt"), "").unwrap();
        std::fs::write(dir.join("PresentMon-2.3.0-x64.exe"), "").unwrap();
        let found = find_presentmon(None, std::slice::from_ref(&dir)).unwrap();
        assert_eq!(found.file_name().unwrap(), "PresentMon-2.3.0-x64.exe");

        let explicit = dir.join("readme.txt");
        assert_eq!(find_presentmon(explicit.to_str(), &[]).unwrap(), explicit);
        assert!(find_presentmon(Some("Z:\\nope\\pm.exe"), std::slice::from_ref(&dir))
            .unwrap_err()
            .contains("not found at"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
