//! Pinfold: persistent per-process CPU affinity for Windows.
//!
//! No arguments: background instance with tray icon.
//! `add`, `remove`, `list`, `topology`: short instances that only read or
//! write the config. The background instance then reads it again.
//! `run`: the engine in a console, without tray (milestone M0).

#![windows_subsystem = "windows"]

mod autostart;
mod bench;
mod config;
mod diag;
mod engine;
mod frametime;
mod log;
mod shortcut;
mod sound;
mod sys;
mod topology;
mod tray;
mod win;

use config::{Config, Mode};
use win::{file_name, message_box, say};

const USAGE: &str = "\
Pinfold - persistent per-process CPU affinity

  Pinfold.exe                      run in the background with a tray icon
  Pinfold.exe add <exe|lnk> [--profile <name>] [--mode affinity|cpusets|both] [--full-path]
                                   create or update a rule (default profile: Cache)
  Pinfold.exe remove <exe>         remove a rule
  Pinfold.exe list                 show profiles and rules
  Pinfold.exe topology             show CPU topology and environment
  Pinfold.exe run                  run the engine in this console, no tray
  Pinfold.exe autostart [on|off]   show or change starting with Windows
  Pinfold.exe quit                 stop the background instance
  Pinfold.exe benchmark <exe> [--profiles A,B] [--seconds 60] [--rounds 2] [--apply]
                                   compare frame times of a running game between
                                   profiles (needs PresentMon and admin rights);
                                   --apply makes a clear winner the rule
  Pinfold.exe sounds               play the four benchmark sounds
  Pinfold.exe <exe-or-shortcut>    pick a profile for the program (drag and drop)

Rules match by file name unless --full-path is given.
Config: %AppData%\\Pinfold\\config.json";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => tray::run(),
        Some(
            cmd @ ("run" | "add" | "remove" | "list" | "topology" | "autostart" | "quit"
            | "benchmark" | "sounds" | "help" | "--help" | "-h" | "/?"),
        ) => {
            if args.iter().any(|a| a == "--window") {
                win::console_window("Pinfold benchmark");
            } else {
                win::console_init();
            }
            match cmd {
                "run" => cmd_run(),
                "add" => cmd_add(&args[1..]),
                "remove" => cmd_remove(&args[1..]),
                "list" => cmd_list(),
                "topology" => cmd_topology(),
                "autostart" => cmd_autostart(&args[1..]),
                "quit" => cmd_quit(),
                "benchmark" => cmd_benchmark(&args[1..]),
                "sounds" => cmd_sounds(),
                _ => {
                    say(USAGE);
                    0
                }
            }
        }
        Some(path) if args.len() == 1 => cmd_drop(path),
        Some(_) => {
            win::console_init();
            say(USAGE);
            2
        }
    };
    std::process::exit(code);
}

/// Loads the config for a short instance. Makes it at the first start.
/// If the config is not correct, tell the user and do not change it: the
/// user can be in the middle of an edit.
fn load_config(gui: bool) -> Option<Config> {
    let dir = config::dir();
    if !config::path(&dir).exists() {
        return Some(config::load_or_create(&dir, &topology::read()).0);
    }
    match config::load(&dir) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            let msg = format!("{e}\nFix or delete {} and try again.", config::path(&dir).display());
            if gui {
                message_box(&msg);
            } else {
                say(&msg);
            }
            None
        }
    }
}

fn cmd_run() -> i32 {
    log::set_echo(true);
    let mut engine = engine::Engine::new(sys::RealSystem, config::dir(), topology::read());
    diag::log_startup(&engine.topo);
    say(&format!("config: {}", config::path(&config::dir()).display()));
    say("polling; press Ctrl+C to stop");
    loop {
        engine.tick();
        std::thread::sleep(std::time::Duration::from_millis(engine.cfg.poll_ms() as u64));
    }
}

/// The rule pattern for a path from the user.
fn pattern_for(path: &str, full_path: bool) -> String {
    if full_path {
        std::path::absolute(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string())
    } else {
        file_name(path).to_string()
    }
}

fn cmd_add(args: &[String]) -> i32 {
    let mut path = None;
    let mut profile = None;
    let mut mode = None;
    let mut full_path = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => profile = it.next().cloned(),
            "--mode" => match it.next().and_then(|m| Mode::parse(m)) {
                Some(m) => mode = Some(m),
                None => {
                    say("--mode must be affinity, cpusets or both");
                    return 2;
                }
            },
            "--full-path" => full_path = true,
            _ if path.is_none() => path = Some(a.clone()),
            _ => {
                say(USAGE);
                return 2;
            }
        }
    }
    let Some(path) = path else {
        say(USAGE);
        return 2;
    };
    // For a shortcut, use the program that it starts.
    let path = match shortcut::program_for(&path) {
        Ok(p) => p,
        Err(e) => {
            say(&e);
            return 2;
        }
    };
    let Some(mut cfg) = load_config(false) else {
        return 1;
    };
    let wanted = profile.unwrap_or_else(|| "Cache".to_string());
    let Some(profile) = cfg.profile(&wanted).map(|p| p.name.clone()) else {
        let names: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
        say(&format!("unknown profile '{wanted}'. Available: {}", names.join(", ")));
        return 2;
    };
    let pattern = pattern_for(&path, full_path);
    let created = cfg.upsert_rule(&pattern, &profile, mode);
    if let Err(e) = config::save(&config::dir(), &cfg) {
        say(&format!("cannot write config: {e}"));
        return 1;
    }
    say(&format!("{} rule: {pattern} -> {profile}", if created { "added" } else { "updated" }));
    0
}

fn cmd_remove(args: &[String]) -> i32 {
    let Some(arg) = args.first() else {
        say(USAGE);
        return 2;
    };
    let Some(mut cfg) = load_config(false) else {
        return 1;
    };
    let index = cfg.rule_index(arg).or_else(|| cfg.rule_index(file_name(arg)));
    let Some(i) = index else {
        say(&format!("no rule for '{arg}'"));
        return 1;
    };
    let removed = cfg.rules.remove(i);
    if let Err(e) = config::save(&config::dir(), &cfg) {
        say(&format!("cannot write config: {e}"));
        return 1;
    }
    say(&format!("removed rule: {}", removed.pattern));
    0
}

fn cmd_list() -> i32 {
    let Some(cfg) = load_config(false) else {
        return 1;
    };
    say(&format!("config: {}", config::path(&config::dir()).display()));
    say("profiles:");
    for p in &cfg.profiles {
        say(&format!("  {:<12} cpus {}", p.name, p.cpus));
    }
    say("rules:");
    if cfg.rules.is_empty() {
        say("  (none)");
    }
    for r in &cfg.rules {
        say(&format!(
            "  {} -> {}  mode={} enforce={}",
            r.pattern,
            r.profile,
            r.mode.as_str(),
            r.enforce
        ));
    }
    0
}

fn cmd_topology() -> i32 {
    let topo = topology::read();
    for line in diag::snapshot(&topo).iter().skip(1) {
        say(line);
    }
    say("default profiles:");
    for p in topo.default_profiles() {
        say(&format!("  {:<12} cpus {}", p.name, p.cpus));
    }
    0
}

/// `autostart` shows the state. `on` and `off` change it. The elevated
/// variant shows one UAC prompt.
fn cmd_autostart(args: &[String]) -> i32 {
    let result = match args.first().map(String::as_str) {
        None => Ok(autostart::status()),
        Some("on") => {
            let Some(cfg) = load_config(false) else {
                return 1;
            };
            autostart::enable(cfg.elevated_autostart)
        }
        Some("off") => autostart::disable(),
        Some(_) => {
            say(USAGE);
            return 2;
        }
    };
    match result {
        Ok(kind) => {
            say(&format!("autostart: {}", kind.describe()));
            0
        }
        Err(e) => {
            say(&e);
            1
        }
    }
}

/// Compare frame times of a running program between profiles.
/// Play the benchmark sounds one after the other, with their meaning.
fn cmd_sounds() -> i32 {
    use bench::Sound;
    for (sound, meaning) in [
        (Sound::Start, "start: the first capture starts"),
        (Sound::Switch, "switch: the next profile starts"),
        (Sound::Problem, "problem: the game is not in front, or an error"),
        (Sound::Done, "done: the result is ready"),
    ] {
        say(meaning);
        bench::play(sound);
        let rest = 800u64.saturating_sub(sound::length_ms(sound) as u64);
        std::thread::sleep(std::time::Duration::from_millis(rest));
    }
    0
}

fn cmd_benchmark(args: &[String]) -> i32 {
    let opts = match bench::parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            say(&e);
            say("");
            say(USAGE);
            return 2;
        }
    };
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_ctrl), 1);
    }
    let code = match bench::run(&opts, &ConsoleUi, &CANCEL) {
        Err(bench::Failure::Cancelled) => {
            say("Benchmark cancelled. The rule is as it was before.");
            1
        }
        Ok(outcome) => {
            say("");
            say(&outcome.report);
            offer_winner(&opts, &outcome);
            0
        }
        Err(bench::Failure::NeedsElevation) => {
            // PresentMon needs administrator rights. Continue in an
            // elevated copy of this command, in a new window.
            say("PresentMon needs administrator rights. Continuing in an elevated window...");
            let mut elevated = opts.to_args();
            if !opts.window {
                elevated.push("--window".to_string());
            }
            elevated.insert(0, "benchmark".to_string());
            let started = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| win::launch_elevated(&exe, &elevated));
            match started {
                // The elevated window continues. This one can close.
                Ok(()) => return 0,
                Err(e) => {
                    say(&format!("Benchmark stopped: {e}"));
                    1
                }
            }
        }
        Err(bench::Failure::Message(e)) => {
            say(&format!("Benchmark stopped: {e}"));
            1
        }
    };
    if opts.window {
        win::wait_for_enter();
    }
    code
}

static CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ctrl+C or a close of the window stops the benchmark. The benchmark then
/// puts the rule back. If not, the rule keeps the profile under test.
unsafe extern "system" fn on_console_ctrl(event: u32) -> windows_sys::Win32::Foundation::BOOL {
    CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
    if event == windows_sys::Win32::System::Console::CTRL_CLOSE_EVENT {
        // The process stops when this function returns. Give the benchmark
        // time to put the rule back.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    1
}

/// The benchmark in a console: lines on stdout, progress in the title.
struct ConsoleUi;

impl bench::Ui for ConsoleUi {
    fn say(&self, line: &str) {
        say(line);
    }
    fn status(&self, text: &str) {
        let title = win::wide(&format!("Pinfold benchmark - {text}"));
        unsafe { windows_sys::Win32::System::Console::SetConsoleTitleW(title.as_ptr()) };
    }
    fn sound(&self, sound: bench::Sound) {
        bench::play(sound);
    }
}

/// After a clear result: make the winner the rule, if the user agrees.
fn offer_winner(opts: &bench::Options, outcome: &bench::Outcome) {
    let target = &opts.target;
    let Some(winner) = outcome.better_profile() else {
        if let Some(w) = &outcome.verdict.winner {
            say(&format!("{target} already uses {w}: nothing to change."));
        }
        return;
    };
    let yes = opts.apply || {
        say(&format!("Make {winner} the rule for {target}? [Y/n]"));
        let mut line = String::new();
        // No answer (no console, end of input) means no.
        matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0)
            && matches!(line.trim().to_lowercase().as_str(), "" | "y" | "yes")
    };
    if !yes {
        say("The rule is unchanged.");
        return;
    }
    match bench::apply(target, winner) {
        Ok(()) => say(&format!("{target} now uses {winner}.")),
        Err(e) => say(&format!("Could not change the rule: {e}")),
    }
}

fn cmd_quit() -> i32 {
    if !tray::is_running() {
        say("Pinfold is not running");
        return 0;
    }
    if tray::quit_running_instance() {
        say("Pinfold stopped");
        0
    } else {
        say("Pinfold did not stop in time");
        1
    }
}

/// Started with one path: the user dropped a file on the executable.
/// Show the profiles at the mouse position, write the rule, stop.
fn cmd_drop(path: &str) -> i32 {
    let path = match shortcut::program_for(path) {
        Ok(p) => p,
        Err(e) => {
            message_box(&e);
            return 2;
        }
    };
    let Some(mut cfg) = load_config(true) else {
        return 1;
    };
    let name = file_name(&path).to_string();
    let profiles: Vec<String> = cfg.profiles.iter().map(|p| p.name.clone()).collect();
    let picked = unsafe {
        let hwnd = tray::hidden_window();
        tray::pick_profile(hwnd, &name, &profiles)
    };
    let Some(profile) = picked else { return 0 };
    cfg.upsert_rule(&name, &profile, None);
    if let Err(e) = config::save(&config::dir(), &cfg) {
        message_box(&format!("Cannot write config: {e}"));
        return 1;
    }
    0
}
