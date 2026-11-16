//! `Pinfold.exe benchmark`: switch a running program between profiles,
//! capture frame times with PresentMon in each, and compare.
//!
//! The switch goes through the rule in the config, so a running background
//! instance applies it (and does not treat it as drift). Without a
//! background instance the benchmark applies the affinity itself.
//! Afterwards the rule is put back the way it was.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;

use crate::config::{self, Config, Mode, Rule};
use crate::engine::System;
use crate::frametime::{self, Stats};
use crate::sys::{affinity_of, pids_by_name, RealSystem};
use crate::topology::{self, format_mask, Topology};
use crate::win::{file_name, say};

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
    /// Started from the tray menu: own console window, wait before closing.
    pub window: bool,
}

impl Options {
    /// The command line that reproduces these options.
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
        countdown: 10,
        presentmon: None,
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

/// Capture order: every profile once per round, and the order reversed in
/// every second round, so that warm-up and slow changes in the scene do
/// not favour whichever profile always comes first.
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

/// "Cache" and "Frequency" when both exist, otherwise every profile.
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

/// Explicit path first; then a PresentMon*.exe next to Pinfold.exe or in
/// the config directory (release files carry a version in their name);
/// then PresentMon.exe on PATH.
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

struct Run<'a> {
    opts: &'a Options,
    dir: PathBuf,
    cfg: Config,
    topo: Topology,
    presentmon: PathBuf,
    out_dir: PathBuf,
    /// A background instance applies config changes; otherwise we do.
    background: bool,
}

fn beep() {
    unsafe { MessageBeep(0) };
}

fn sleep_secs(s: u32) {
    std::thread::sleep(Duration::from_secs(s as u64));
}

impl Run<'_> {
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

    /// Point the rule at `profile` and wait until the running program has it.
    fn switch(&mut self, profile: &str) -> Result<(), String> {
        let mask = self.mask_of(profile)?;
        self.cfg.upsert_rule(&self.opts.target, profile, None);
        config::save(&self.dir, &self.cfg).map_err(|e| format!("cannot write config: {e}"))?;
        let mode = self.mode();
        if !self.background {
            self.apply_directly(mask, mode);
        }
        if mode == Mode::Cpusets {
            // CPU Sets cannot be read back without opening the process for
            // more rights than a query; give the engine one poll.
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

    fn capture(&self, round: u32, profile: &str) -> Result<Vec<f64>, String> {
        let safe: String =
            profile.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let csv = self.out_dir.join(format!("round{round}-{safe}.csv"));
        let _ = std::fs::remove_file(&csv);
        presentmon_capture(&self.presentmon, &self.opts.target, &csv, self.opts.seconds)
            .map_err(|e| e.message())?;
        let text = std::fs::read_to_string(&csv)
            .map_err(|_| format!("PresentMon wrote no capture file ({})", csv.display()))?;
        frametime::parse_csv(&text).map_err(|e| {
            format!("{e} ({}). Was {} in front and rendering?", csv.display(), self.opts.target)
        })
    }
}

/// Why a PresentMon run failed, with what PresentMon itself said.
pub struct CaptureError {
    code: Option<i32>,
    output: String,
}

impl CaptureError {
    /// PresentMon could not start its trace session for lack of rights.
    pub fn is_access_denied(&self) -> bool {
        self.output.to_lowercase().contains("access denied")
    }

    pub fn message(&self) -> String {
        let said = self.output.trim();
        let code = self.code.map(|c| c.to_string()).unwrap_or_else(|| "none".into());
        if said.is_empty() {
            format!("PresentMon failed (exit code {code}) without a message.")
        } else {
            format!("PresentMon failed (exit code {code}):\n{said}")
        }
    }
}

/// One timed PresentMon capture of all processes named `target` into `csv`.
fn presentmon_capture(
    presentmon: &Path,
    target: &str,
    csv: &Path,
    seconds: u32,
) -> Result<(), CaptureError> {
    let fail = |code: Option<i32>, output: String| CaptureError { code, output };
    // PresentMon reports problems on stderr; keep it for the error message.
    let err_path = csv.with_extension("presentmon.txt");
    let err_file = std::fs::File::create(&err_path).map_err(|e| fail(None, e.to_string()))?;
    let mut child = Command::new(presentmon)
        .args(["--process_name", target])
        .arg("--output_file")
        .arg(csv)
        .args(["--timed", &seconds.to_string()])
        .args(["--terminate_after_timed", "--no_console_stats"])
        .args(["--session_name", "PinfoldBenchmark", "--stop_existing_session"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .spawn()
        .map_err(|e| fail(None, format!("cannot start {}: {e}", presentmon.display())))?;
    let deadline = Instant::now() + Duration::from_secs(seconds as u64 + 30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return Err(fail(None, "PresentMon did not finish in time".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(fail(None, format!("waiting for PresentMon failed: {e}"))),
        }
    };
    let output = std::fs::read_to_string(&err_path).unwrap_or_default();
    if status.success() {
        let _ = std::fs::remove_file(&err_path);
        Ok(())
    } else {
        Err(fail(status.code(), output))
    }
}

/// Why the benchmark did not produce a report.
pub enum Failure {
    /// PresentMon needs rights this process does not have; running the
    /// benchmark elevated will help.
    NeedsElevation,
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
    // Re-read: only our rule is touched, whatever else changed stays.
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
    // Nobody else resets a program that had no rule and no engine watching.
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

pub fn run(opts: &Options) -> Result<String, Failure> {
    let dir = config::dir();
    let cfg = config::load(&dir)?;
    let topo = topology::read();
    let profiles: Vec<String> = if opts.profiles.is_empty() {
        default_profiles(&cfg)
    } else {
        opts.profiles
            .iter()
            .map(|p| cfg.profile(p).map(|p| p.name.clone()).ok_or(format!("unknown profile '{p}'")))
            .collect::<Result<_, _>>()?
    };
    if profiles.len() < 2 {
        return Err("a comparison needs at least two profiles".into());
    }
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

    // One-second trial capture before anyone switches to the game: finds
    // missing rights and other PresentMon problems while they are cheap.
    let trial = out_dir.join("trial.csv");
    let checked = presentmon_capture(&presentmon, &opts.target, &trial, 1);
    let _ = std::fs::remove_file(&trial);
    if let Err(e) = checked {
        let _ = std::fs::remove_dir_all(&out_dir);
        return Err(if e.is_access_denied() && !crate::diag::is_elevated() {
            Failure::NeedsElevation
        } else {
            Failure::Message(e.message())
        });
    }

    let original = cfg.rule_index(&opts.target).map(|i| cfg.rules[i].clone());
    let background = crate::tray::is_running();
    let plan = phases(&profiles, opts.rounds);
    let total = opts.countdown + plan.len() as u32 * (opts.settle + opts.seconds);
    say(&format!("Benchmark of {}: {}", opts.target, profiles.join(" vs ")));
    say(&format!(
        "{} captures of {} s, about {} min {} s in total. PresentMon: {}",
        plan.len(),
        opts.seconds,
        total / 60,
        total % 60,
        presentmon.display()
    ));
    say("Keep the game in front and the scene as constant as you can.");
    say("A sound marks the start, every switch and the end.");
    if opts.countdown > 0 {
        say(&format!("Starting in {} s - switch to the game now.", opts.countdown));
        sleep_secs(opts.countdown);
    }

    let mut run = Run { opts, dir: dir.clone(), cfg, topo, presentmon, out_dir, background };
    let mut frames: Vec<(String, Vec<f64>)> =
        profiles.iter().map(|p| (p.clone(), Vec::new())).collect();
    let mut rounds_text = String::new();
    let mut outcome = Ok(());
    for (round, profile) in &plan {
        beep();
        let step = (|| {
            run.switch(profile)?;
            let mask = run.mask_of(profile)?;
            say(&format!("round {round}: {profile} (cpus {}) ...", format_mask(mask)));
            sleep_secs(opts.settle);
            let times = run.capture(*round, profile)?;
            let s = frametime::stats(&times).ok_or("the capture contains no frames")?;
            let line = format!(
                "round {round}: {profile:<12} {:>8.1} avg FPS  {:>8.1} 1% low  {:>7.2} ms p99  ({} frames)",
                s.avg_fps, s.low1_fps, s.p99_ms, s.frames
            );
            say(&line);
            rounds_text.push_str(&line);
            rounds_text.push('\n');
            if let Some((_, all)) = frames.iter_mut().find(|(p, _)| p == profile) {
                all.extend(times);
            }
            Ok::<(), String>(())
        })();
        if let Err(e) = step {
            outcome = Err(e);
            break;
        }
    }
    let reset = (!background && original.is_none()).then_some(run.topo.all_mask);
    restore(&dir, &opts.target, original, reset);
    beep();
    outcome.map_err(Failure::Message)?;

    let columns: Vec<(String, Stats)> = frames
        .iter()
        .filter_map(|(p, times)| frametime::stats(times).map(|s| (p.clone(), s)))
        .collect();
    let mut text = format!(
        "Pinfold benchmark of {}\n{} rounds, {} s per capture, all rounds of a profile combined.\n\n",
        opts.target, opts.rounds, opts.seconds
    );
    text.push_str(&frametime::report(&columns));
    text.push_str(
        "\nFPS rows: higher is better. Frame time rows: lower is better.\n\
         Differences under about 2-3 % are within run-to-run noise.\n\nPer round:\n",
    );
    text.push_str(&rounds_text);
    let report_path = run.out_dir.join("report.txt");
    let _ = std::fs::write(&report_path, &text);
    text.push_str(&format!("\nCaptures and this report: {}\n", run.out_dir.display()));
    Ok(text)
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
        assert_eq!((o.seconds, o.rounds, o.settle, o.countdown), (60, 2, 5, 10));
        assert!(o.profiles.is_empty() && o.presentmon.is_none() && !o.window);
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
            "--window",
        ]))
        .unwrap();
        assert_eq!(parse_args(&o.to_args()).unwrap(), o);
        let plain = parse_args(&args(&["game.exe"])).unwrap();
        assert_eq!(parse_args(&plain.to_args()).unwrap(), plain);
    }

    #[test]
    fn access_denied_is_recognised_from_what_presentmon_says() {
        let denied = CaptureError {
            code: Some(6),
            output: "error: failed to start trace session: access denied.\n".into(),
        };
        assert!(denied.is_access_denied());
        assert!(denied.message().contains("exit code 6"));
        assert!(denied.message().contains("failed to start trace session"));
        let other = CaptureError { code: Some(5), output: String::new() };
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
