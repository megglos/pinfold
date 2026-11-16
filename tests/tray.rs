//! The background (tray) instance: started for real, in a private config
//! directory. The menu itself needs a person and is not covered here.

mod common;
use common::{wait_for, Sandbox, TIMEOUT};

#[test]
fn quit_reports_when_nothing_is_running() {
    let s = Sandbox::new();
    assert!(s.ok(&["quit"]).contains("not running"));
}

#[test]
fn tray_instance_pins_and_quits_on_request() {
    let s = Sandbox::new();
    let target = s.target();
    s.write_config(&format!(r#"{{ "match": "{}", "profile": "One" }}"#, target.name));
    let mut tray = s.start_tray();
    wait_for("tray start", TIMEOUT, || s.log().contains("started"));

    // The tray instance drives the engine from its window timer.
    let p = target.start();
    wait_for("pin", TIMEOUT, || p.affinity() == 1);

    assert!(s.ok(&["quit"]).contains("Pinfold stopped"));
    let status = tray.0.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    assert!(s.log().trim_end().ends_with("exit"), "{}", s.log());

    // Quitting leaves the pin in place.
    assert_eq!(p.affinity(), 1);
}

#[test]
fn tray_instances_with_different_config_dirs_do_not_see_each_other() {
    let a = Sandbox::new();
    let b = Sandbox::new();
    a.write_config("");
    b.write_config("");
    let mut tray_a = a.start_tray();
    let mut tray_b = b.start_tray();
    wait_for("both started", TIMEOUT, || {
        a.log().contains("started") && b.log().contains("started")
    });
    assert!(a.ok(&["quit"]).contains("stopped"));
    assert_eq!(tray_a.0.wait().unwrap().code(), Some(0));
    assert!(tray_b.0.try_wait().unwrap().is_none(), "b must still be running");
    assert!(b.ok(&["quit"]).contains("stopped"));
    assert_eq!(tray_b.0.wait().unwrap().code(), Some(0));
}
