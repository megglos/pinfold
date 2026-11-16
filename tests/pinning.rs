//! End-to-end tests: the real engine (`Pinfold.exe run`) pins real
//! processes. Each test uses a target program with a unique file name.
//! Thus tests can run in parallel and do not affect other programs.

mod common;
use common::{multi_cpu, wait_for, Sandbox, TIMEOUT};

const CPU0: usize = 1;

fn rule(name: &str, extra: &str) -> String {
    format!(r#"{{ "match": "{name}", "profile": "One"{extra} }}"#)
}

#[test]
fn new_process_is_pinned_and_logged() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, ""));
    let _engine = s.start_engine();
    wait_for("engine start", TIMEOUT, || s.log().contains("started"));

    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
    wait_for("log entry", TIMEOUT, || s.log().contains("applied"));
    let log = s.log();
    assert!(
        log.contains(&format!("applied {} pid {} -> One (cpus 0)", target.name, p.0.id())),
        "{log}"
    );
}

#[test]
fn process_already_running_at_startup_is_pinned() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, ""));
    let p = target.start();
    let _engine = s.start_engine();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
}

#[test]
fn restarted_program_is_pinned_again() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, ""));
    let _engine = s.start_engine();
    {
        let first = target.start();
        wait_for("first pin", TIMEOUT, || first.affinity() == CPU0);
    }
    let second = target.start();
    wait_for("second pin", TIMEOUT, || second.affinity() == CPU0);
}

#[test]
fn full_path_rule_matches() {
    let s = Sandbox::new();
    let target = s.target();
    let path = target.path.to_string_lossy().replace('\\', "\\\\");
    s.write_config(&rule(&path, ""));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
}

#[test]
fn drift_is_restored_and_logged() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, ""));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
    if !multi_cpu(&p) {
        return;
    }
    p.set_affinity(p.system_mask());
    wait_for("restore", TIMEOUT, || p.affinity() == CPU0);
    wait_for("drift log", TIMEOUT, || s.log().contains("drift"));
    assert!(s.log().contains("expected cpus 0, found affinity"), "{}", s.log());
}

#[test]
fn repeated_drift_backs_off_as_contested() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, ""));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
    if !multi_cpu(&p) {
        return;
    }
    let all = p.system_mask();
    // The engine corrects five drifts ...
    for i in 0..5 {
        p.set_affinity(all);
        wait_for(&format!("restore {i}"), TIMEOUT, || p.affinity() == CPU0);
    }
    // ... but not the sixth in one minute.
    p.set_affinity(all);
    wait_for("contested log", TIMEOUT, || s.log().contains("contested"));
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(p.affinity(), all, "engine must stop re-applying");
    assert_eq!(s.log().matches("drift ").count(), 5, "{}", s.log());
}

#[test]
fn enforce_false_applies_once_only() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, r#", "enforce": false"#));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
    if !multi_cpu(&p) {
        return;
    }
    p.set_affinity(p.system_mask());
    std::thread::sleep(std::time::Duration::from_millis(1000));
    assert_eq!(p.affinity(), p.system_mask());
    assert!(!s.log().contains("drift"));
}

#[test]
fn rule_added_while_running_is_picked_up() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config("");
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("engine start", TIMEOUT, || s.log().contains("started"));
    if multi_cpu(&p) {
        assert_ne!(p.affinity(), CPU0);
    }
    s.ok(&["add", &target.name, "--profile", "One"]);
    wait_for("pin after config reload", TIMEOUT, || p.affinity() == CPU0);
    assert!(s.log().contains("config reloaded: 1 rule(s)"), "{}", s.log());
}

#[test]
fn removing_the_rule_resets_the_process_to_all_cpus() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, r#", "mode": "both""#));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0 && p.cpu_set_count() == 1);
    s.ok(&["remove", &target.name]);
    wait_for("reset", TIMEOUT, || p.affinity() == p.system_mask() && p.cpu_set_count() == 0);
    assert!(s.log().contains("to all cores (rule removed)"), "{}", s.log());
}

#[test]
fn mode_cpusets_sets_cpu_sets_and_leaves_the_mask() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, r#", "mode": "cpusets""#));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("cpu sets", TIMEOUT, || p.cpu_set_count() == 1);
    assert_eq!(p.affinity(), p.system_mask(), "affinity mask must be untouched");
}

#[test]
fn broken_config_edit_keeps_enforcing_the_previous_rules() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&rule(&target.name, ""));
    let _engine = s.start_engine();
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == CPU0);
    std::fs::write(s.config_path(), "{ broken").unwrap();
    wait_for("error log", TIMEOUT, || s.log().contains("keeping previous config"));
    if !multi_cpu(&p) {
        return;
    }
    p.set_affinity(p.system_mask());
    wait_for("restore", TIMEOUT, || p.affinity() == CPU0);
}

#[test]
fn startup_logs_the_environment_once() {
    let s = Sandbox::new();
    s.write_config("");
    let _engine = s.start_engine();
    wait_for("env log", TIMEOUT, || s.log().contains("Windows Game Mode"));
    let log = s.log();
    assert_eq!(log.matches("topology: ").count(), 1, "{log}");
    assert!(log.contains("amd3dvcache"), "{log}");
}
