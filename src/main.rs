//! Pinfold: persistent per-process CPU affinity for Windows.
//!
//! No arguments: background instance with tray icon.
//! `add`, `remove`, `list`, `topology`: short-lived instances that only
//! read or write the config; the background instance reloads it.
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
  Pinfold.exe <exe-or-shortcut>    pick a profile for the program (drag and drop)

Rules match by file name unless --full-path is given.
Config: %AppData%\\Pinfold\\config.json";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => tray::run(),
        Some(
            cmd @ ("run" | "add" | "remove" | "list" | "topology" | "autostart" | "quit"
            | "benchmark" | "help" | "--help" | "-h" | "/?"),
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

/// Loads the config for a short-lived instance, creating it on first run.
/// A config that exists but cannot be parsed is reported and left alone,
/// so that a command never overwrites a file the user is in the middle of
/// editing.
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

/// The rule pattern for a path given by the user.
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
    // A shortcut is resolved to the program it starts.
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

/// `autostart` shows the state; `on` and `off` change it. The elevated
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
    let code = match bench::run(&opts) {
        Ok(outcome) => {
            say("");
            say(&outcome.report);
            offer_winner(&opts, &outcome);
            0
        }
        Err(bench::Failure::NeedsElevation) => {
            // PresentMon needs administrator rights: continue in an
            // elevated copy of this command, in its own window.
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
                // The elevated window takes over; this one can close.
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
        // No answer at all (no console, end of input) is a no.
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

/// Started with a single path: something was dropped onto the executable.
/// Show a profile picker at the mouse position, write the rule, exit.
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
