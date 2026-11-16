//! `Pinfold.exe benchmark` end to end, with a stand-in for PresentMon.
//!
//! The stand-in looks at the affinity the target process has while it is
//! "capturing" and writes fast frames (10 ms) when the process is pinned to
//! cpu 0 and slow frames (20 ms) otherwise. That proves the benchmark
//! captures each profile while that profile is really applied.

mod common;
use common::{multi_cpu, wait_for, Sandbox, TIMEOUT};
use std::path::PathBuf;

/// Profiles "One" (cpu 0) and "Wide" (every cpu the machine has).
fn write_config(s: &Sandbox, rules: &str) {
    let text = format!(
        r#"{{ "pollIntervalMs": 250,
  "profiles": [ {{ "name": "One", "cpus": "0" }}, {{ "name": "Wide", "cpus": "0-63" }} ],
  "rules": [ {rules} ] }}"#
    );
    std::fs::write(s.config_path(), text).unwrap();
}

/// A PresentMon stand-in: a .cmd that runs a PowerShell script.
fn fake_presentmon(s: &Sandbox, body: &str) -> PathBuf {
    let dir = s.dir.join("pm");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fake.ps1"), body).unwrap();
    let cmd = dir.join("PresentMon-fake.cmd");
    std::fs::write(
        &cmd,
        "@powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0fake.ps1\" %*\r\n",
    )
    .unwrap();
    cmd
}

/// Captures every program, like the real PresentMon without a process
/// filter: the desktop compositor plus every test target that is running
/// (from all tests). The benchmark has to pick its own target out.
const CAPTURING: &str = r#"
$out = $args[[array]::IndexOf($args, '--output_file') + 1]
if ($args -notcontains '--timed' -or $args -notcontains '--terminate_after_timed') { exit 3 }
if ($args -contains '--process_name') { exit 4 }
$rows = @('Application,ProcessID,MsBetweenPresents')
$rows += 1..500 | ForEach-Object { "dwm.exe,1,16.6" }
foreach ($p in Get-Process -Name 'pf-target-*' -ErrorAction SilentlyContinue) {
    $ms = if ([int64]$p.ProcessorAffinity -eq 1) { '10.0' } else { '20.0' }
    $rows += 1..200 | ForEach-Object { "$($p.Name).exe,$($p.Id),$ms" }
}
[IO.File]::WriteAllLines($out, $rows)
"#;
fn rule_count(s: &Sandbox) -> usize {
    let cfg: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    cfg["rules"].as_array().unwrap().len()
}

const FAST: &[&str] = &["--seconds", "1", "--settle", "0", "--countdown", "0"];

#[test]
fn compares_profiles_and_leaves_no_trace_when_there_was_no_rule() {
    let s = Sandbox::new();
    write_config(&s, "");
    let pm = fake_presentmon(&s, CAPTURING);
    let target = s.target();
    let p = target.start();
    wait_for("target visible", TIMEOUT, || p.0.id() != 0 && p.affinity() != 0);
    if !multi_cpu(&p) {
        return;
    }

    let mut args = vec!["benchmark", target.name.as_str(), "--presentmon", pm.to_str().unwrap()];
    args.extend_from_slice(FAST);
    let text = s.ok(&args);

    // Default: two rounds, order reversed in the second.
    let rounds: Vec<&str> =
        text.lines().filter(|l| l.starts_with("round") && l.contains("avg FPS")).collect();
    // Printed live and once more in the report.
    assert_eq!(rounds.len(), 8, "{text}");
    assert!(
        rounds[0].starts_with("round 1: One") && rounds[1].starts_with("round 1: Wide"),
        "{text}"
    );
    assert!(
        rounds[2].starts_with("round 2: Wide") && rounds[3].starts_with("round 2: One"),
        "{text}"
    );

    // Each capture saw the affinity of its own profile.
    let line = |label: &str| {
        text.lines().find(|l| l.starts_with(label)).unwrap_or_else(|| panic!("{text}"))
    };
    assert!(line("avg FPS").contains("100.00") && line("avg FPS").contains("50.00"), "{text}");
    assert!(line("avg FPS").contains("-50.0%"), "{text}");
    assert!(line("frames").contains("400"), "both rounds are combined: {text}");

    // Report and captures are kept.
    let bench_dir =
        std::fs::read_dir(s.dir.join("benchmarks")).unwrap().next().unwrap().unwrap().path();
    assert!(bench_dir.join("report.txt").exists());
    let csvs = std::fs::read_dir(&bench_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "csv"))
        .count();
    assert_eq!(csvs, 4);

    // No rule existed before: none afterwards, and the program is unpinned.
    assert_eq!(rule_count(&s), 0);
    assert_eq!(p.affinity(), p.system_mask());
}

#[test]
fn existing_rule_is_restored_and_the_background_instance_does_the_switching() {
    let s = Sandbox::new();
    let target = s.target();
    write_config(
        &s,
        &format!(
            r#"{{ "match": "{}", "profile": "One", "mode": "affinity", "enforce": true }}"#,
            target.name
        ),
    );
    let pm = fake_presentmon(&s, CAPTURING);
    let _tray = s.start_tray();
    let p = target.start();
    wait_for("pin by background instance", TIMEOUT, || p.affinity() == 1);
    if !multi_cpu(&p) {
        return;
    }
    let before = s.config();

    let mut args = vec![
        "benchmark",
        target.name.as_str(),
        "--presentmon",
        pm.to_str().unwrap(),
        "--profiles",
        "Wide,One",
        "--rounds",
        "1",
    ];
    args.extend_from_slice(FAST);
    let text = s.ok(&args);
    let line = |label: &str| {
        text.lines().find(|l| l.starts_with(label)).unwrap_or_else(|| panic!("{text}"))
    };
    // Wide is the baseline here: 50 FPS, One is +100 %.
    assert!(line("avg FPS").contains("+100.0%"), "{text}");

    let after: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    let original: serde_json::Value = serde_json::from_str(&before).unwrap();
    assert_eq!(after["rules"], original["rules"], "the rule must be exactly as before");
    wait_for("pin restored", TIMEOUT, || p.affinity() == 1);
    // The switches went through the config, not past the engine.
    assert!(!s.log().contains("drift"), "{}", s.log());
    s.ok(&["quit"]);
}

#[test]
fn problems_are_explained_and_nothing_is_changed() {
    let s = Sandbox::new();
    write_config(&s, "");
    let pm = fake_presentmon(&s, CAPTURING);
    let pm = pm.to_str().unwrap();
    let before = s.config();
    let text = |args: &[&str]| {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    assert!(text(&["benchmark", "not-running-anywhere.exe", "--presentmon", pm])
        .contains("is not running"));
    assert!(text(&["benchmark", "x.exe", "--presentmon", "Z:\\no\\PresentMon.exe"])
        .contains("not found at"));
    assert!(text(&["benchmark", "x.exe", "--presentmon", pm, "--profiles", "One"])
        .contains("at least two"));
    assert!(text(&["benchmark", "x.exe", "--presentmon", pm, "--profiles", "One,Nope"])
        .contains("unknown profile"));
    assert_eq!(s.run(&["benchmark"]).status.code(), Some(2));
    assert_eq!(s.config(), before);
    assert!(!s.dir.join("benchmarks").exists());
}

#[test]
fn presentmon_failure_is_reported_with_its_own_words_before_anything_is_changed() {
    let s = Sandbox::new();
    let target = s.target();
    write_config(
        &s,
        &format!(r#"{{ "match": "{}", "profile": "Wide", "enforce": false }}"#, target.name),
    );
    let pm =
        fake_presentmon(&s, "[Console]::Error.WriteLine('error: no swap chain for you'); exit 5");
    let p = target.start();
    wait_for("target visible", TIMEOUT, || p.affinity() != 0);

    let mut args = vec!["benchmark", target.name.as_str(), "--presentmon", pm.to_str().unwrap()];
    args.extend_from_slice(FAST);
    let out = s.run(&args);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("exit code 5") && text.contains("no swap chain for you"), "{text}");
    // The trial capture fails before the countdown and before any switch.
    assert!(!text.contains("round 1"), "{text}");
    assert!(!s.log().contains("config reloaded"));

    let cfg: serde_json::Value = serde_json::from_str(&s.config()).unwrap();
    assert_eq!(cfg["rules"][0]["profile"], "Wide");
    assert_eq!(cfg["rules"][0]["enforce"], false);
}

#[test]
fn a_game_that_presents_nothing_is_reported_with_who_did() {
    let s = Sandbox::new();
    write_config(&s, "");
    let pm = fake_presentmon(&s, CAPTURING);
    // Running, but not one of the programs the stand-in sees presenting.
    let target = s.target();
    let quiet = s.dir.join("target").join("quiet-game.exe");
    std::fs::copy(&target.path, &quiet).unwrap();
    let mut child = std::process::Command::new(&quiet)
        .arg("run")
        .env("PINFOLD_DIR", s.dir.join("quiet-cfg"))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mut args = vec!["benchmark", "quiet-game.exe", "--presentmon", pm.to_str().unwrap()];
    args.extend_from_slice(FAST);
    let out = s.run(&args);
    let _ = child.kill();
    let _ = child.wait();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("quiet-game.exe presented no frames"), "{text}");
    assert!(text.contains("dwm.exe (500 frames)"), "{text}");
    assert_eq!(rule_count(&s), 0, "rule removed again");
}
