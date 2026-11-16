//! Pinfold: persistent per-process CPU affinity for Windows.
//!
//! No arguments: background instance with tray icon.
//! `add`, `remove`, `list`, `topology`: short-lived instances that only
//! read or write the config; the background instance reloads it.
//! `run`: the engine in a console, without tray (milestone M0).

#![windows_subsystem = "windows"]

mod config;
mod diag;
mod engine;
mod log;
mod topology;
mod tray;
mod win;

use config::{Config, Mode};
use win::{file_name, message_box, say};

const USAGE: &str = "\
Pinfold - persistent per-process CPU affinity

  Pinfold.exe                      run in the background with a tray icon
  Pinfold.exe add <exe> [--profile <name>] [--mode affinity|cpusets|both] [--full-path]
                                   create or update a rule (default profile: Cache)
  Pinfold.exe remove <exe>         remove a rule
  Pinfold.exe list                 show profiles and rules
  Pinfold.exe topology             show CPU topology and environment
  Pinfold.exe run                  run the engine in this console, no tray
  Pinfold.exe <path-to-exe>        pick a profile for the program (drag and drop)

Rules match by file name unless --full-path is given.
Config: %AppData%\\Pinfold\\config.json";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => tray::run(),
        Some(cmd @ ("run" | "add" | "remove" | "list" | "topology" | "help" | "--help" | "-h" | "/?")) => {
            win::console_init();
            match cmd {
                "run" => cmd_run(),
                "add" => cmd_add(&args[1..]),
                "remove" => cmd_remove(&args[1..]),
                "list" => cmd_list(),
                "topology" => cmd_topology(),
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

fn load_config() -> Config {
    config::load_or_create(&topology::read())
}

fn cmd_run() -> i32 {
    log::set_echo(true);
    let mut engine = engine::Engine::new();
    diag::log_startup(&engine.topo);
    say(&format!("config: {}", config::path().display()));
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
    if !path.to_lowercase().ends_with(".exe") {
        say("only .exe paths or names are supported here");
        return 2;
    }
    let mut cfg = load_config();
    let wanted = profile.unwrap_or_else(|| "Cache".to_string());
    let Some(profile) = cfg.profile(&wanted).map(|p| p.name.clone()) else {
        let names: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
        say(&format!("unknown profile '{wanted}'. Available: {}", names.join(", ")));
        return 2;
    };
    let pattern = pattern_for(&path, full_path);
    let created = cfg.upsert_rule(&pattern, &profile, mode);
    if let Err(e) = config::save(&cfg) {
        say(&format!("cannot write config: {e}"));
        return 1;
    }
    say(&format!(
        "{} rule: {pattern} -> {profile}",
        if created { "added" } else { "updated" }
    ));
    0
}

fn cmd_remove(args: &[String]) -> i32 {
    let Some(arg) = args.first() else {
        say(USAGE);
        return 2;
    };
    let mut cfg = load_config();
    let index = cfg.rule_index(arg).or_else(|| cfg.rule_index(file_name(arg)));
    let Some(i) = index else {
        say(&format!("no rule for '{arg}'"));
        return 1;
    };
    let removed = cfg.rules.remove(i);
    if let Err(e) = config::save(&cfg) {
        say(&format!("cannot write config: {e}"));
        return 1;
    }
    say(&format!("removed rule: {}", removed.pattern));
    0
}

fn cmd_list() -> i32 {
    let cfg = load_config();
    say(&format!("config: {}", config::path().display()));
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

/// Started with a single path: something was dropped onto the executable.
/// Show a profile picker at the mouse position, write the rule, exit.
fn cmd_drop(path: &str) -> i32 {
    let lower = path.to_lowercase();
    if lower.ends_with(".url") {
        message_box(
            "This shortcut does not point at an executable (for example a Steam or Store link), \
             so Pinfold cannot tell which program it starts.\n\n\
             Start the program once and assign it from the tray menu under Assign.",
        );
        return 1;
    }
    if lower.ends_with(".lnk") {
        message_box(
            "Shortcuts are not supported yet in this build.\n\n\
             Drop the .exe itself, or start the program and assign it from the tray menu.",
        );
        return 1;
    }
    if !lower.ends_with(".exe") {
        message_box(&format!("Not an executable:\n{path}\n\n{USAGE}"));
        return 2;
    }
    let mut cfg = load_config();
    let name = file_name(path).to_string();
    let profiles: Vec<String> = cfg.profiles.iter().map(|p| p.name.clone()).collect();
    let picked = unsafe {
        let hwnd = tray::hidden_window();
        tray::pick_profile(hwnd, &name, &profiles)
    };
    let Some(profile) = picked else { return 0 };
    cfg.upsert_rule(&name, &profile, None);
    if let Err(e) = config::save(&cfg) {
        message_box(&format!("Cannot write config: {e}"));
        return 1;
    }
    0
}
