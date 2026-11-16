//! Integration tests for the command line: the real binary, a private
//! config directory, no processes pinned.

mod common;
use common::Sandbox;

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn help_prints_usage() {
    let s = Sandbox::new();
    let text = s.ok(&["help"]);
    assert!(text.contains("Pinfold.exe add <exe>"), "{text}");
    assert!(!s.config_path().exists(), "help must not create a config");
}

#[test]
fn unknown_arguments_print_usage_and_fail() {
    let s = Sandbox::new();
    let out = s.run(&["frobnicate", "now"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).contains("Pinfold.exe list"));
}

#[test]
fn topology_reports_this_machine() {
    let s = Sandbox::new();
    let text = s.ok(&["topology"]);
    assert!(text.contains("topology: "), "{text}");
    assert!(text.contains("logical"), "{text}");
    assert!(text.contains("Windows Game Mode"), "{text}");
    assert!(text.contains("All cores"), "{text}");
}

#[test]
fn first_run_creates_a_config_with_default_profiles() {
    let s = Sandbox::new();
    let text = s.ok(&["list"]);
    assert!(text.contains("All cores"), "{text}");
    assert!(text.contains("(none)"), "{text}");
    let cfg: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    assert_eq!(cfg["pollIntervalMs"], 1000);
    assert!(cfg["profiles"].as_array().unwrap().iter().any(|p| p["name"] == "All cores"));
    assert_eq!(cfg["rules"].as_array().unwrap().len(), 0);
}

#[test]
fn add_list_remove_round_trip() {
    let s = Sandbox::new();
    s.write_config("");
    let text = s.ok(&["add", "C:\\Games\\Foo\\Foo.exe", "--profile", "One"]);
    assert!(text.contains("added rule: Foo.exe -> One"), "{text}");

    let cfg: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    let rule = &cfg["rules"][0];
    assert_eq!(rule["match"], "Foo.exe", "rules match by file name by default");
    assert_eq!(rule["profile"], "One");
    assert_eq!(rule["mode"], "affinity");
    assert_eq!(rule["enforce"], true);

    assert!(s.ok(&["list"]).contains("Foo.exe -> One  mode=affinity enforce=true"));

    let text = s.ok(&["remove", "foo.exe"]);
    assert!(text.contains("removed rule: Foo.exe"), "{text}");
    assert!(s.ok(&["list"]).contains("(none)"));
}

#[test]
fn adding_twice_updates_instead_of_duplicating() {
    let s = Sandbox::new();
    s.write_config("");
    s.ok(&["add", "foo.exe", "--profile", "One"]);
    let text = s.ok(&["add", "FOO.EXE", "--profile", "one", "--mode", "both"]);
    assert!(text.contains("updated rule"), "{text}");
    let cfg: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    assert_eq!(cfg["rules"].as_array().unwrap().len(), 1);
    assert_eq!(cfg["rules"][0]["mode"], "both");
    assert_eq!(cfg["rules"][0]["profile"], "One", "profile name is stored as defined");
}

#[test]
fn full_path_flag_stores_the_path() {
    let s = Sandbox::new();
    s.write_config("");
    s.ok(&["add", "C:\\Games\\Foo\\foo.exe", "--profile", "One", "--full-path"]);
    let cfg: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    assert_eq!(cfg["rules"][0]["match"], "C:\\Games\\Foo\\foo.exe");
}

#[test]
fn unknown_profile_is_rejected_and_lists_the_available_ones() {
    let s = Sandbox::new();
    s.write_config("");
    let before = s.config();
    let out = s.run(&["add", "foo.exe", "--profile", "Nope"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).contains("Available: One"), "{}", stdout(&out));
    assert_eq!(s.config(), before, "config must be untouched");
}

#[test]
fn bad_mode_and_non_exe_are_rejected() {
    let s = Sandbox::new();
    s.write_config("");
    assert_eq!(
        s.run(&["add", "foo.exe", "--profile", "One", "--mode", "x"]).status.code(),
        Some(2)
    );
    assert_eq!(s.run(&["add", "foo.txt", "--profile", "One"]).status.code(), Some(2));
    assert_eq!(s.run(&["add"]).status.code(), Some(2));
}

#[test]
fn removing_a_missing_rule_fails() {
    let s = Sandbox::new();
    s.write_config("");
    let out = s.run(&["remove", "nothing.exe"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("no rule for"));
}

#[test]
fn broken_config_is_reported_and_never_overwritten() {
    let s = Sandbox::new();
    std::fs::write(s.config_path(), "{ half an edit").unwrap();
    for args in [&["list"][..], &["add", "foo.exe", "--profile", "One"], &["remove", "foo.exe"]] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(stdout(&out).contains("config.json"), "{}", stdout(&out));
        assert_eq!(s.config(), "{ half an edit", "{args:?} changed the file");
    }
}

#[test]
fn config_with_utf8_bom_is_accepted() {
    let s = Sandbox::new();
    s.write_config("");
    let with_bom = format!("\u{feff}{}", s.config());
    std::fs::write(s.config_path(), with_bom).unwrap();
    assert!(s.ok(&["list"]).contains("One"));
}
