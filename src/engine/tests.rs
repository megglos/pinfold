//! Engine tests against a fake system: no real processes are touched.
//! Each test gets its own config directory under the temp dir.

use super::*;
use crate::config::{Profile, Rule};
use crate::topology::L3;
use std::sync::atomic::{AtomicU32, Ordering};

const ALL: u64 = 0xFFFF_FFFF;
const CACHE: u64 = 0x0000_FFFF;
const FREQ: u64 = 0xFFFF_0000;
const ACCESS_DENIED: u32 = 5;

struct FakeProc {
    pid: u32,
    start: u64,
    path: String,
    affinity: u64,
    sets: Vec<u32>,
    deny_open: bool,
    deny_set: bool,
}

#[derive(Default)]
struct Fake {
    procs: Vec<FakeProc>,
    logs: Vec<String>,
    info_calls: usize,
    open_handles: Vec<Handle>,
    spawned: u64,
}

impl Fake {
    fn spawn(&mut self, pid: u32, path: &str) {
        self.spawned += 1;
        self.procs.push(FakeProc {
            pid,
            start: self.spawned,
            path: path.to_string(),
            affinity: ALL,
            sets: Vec::new(),
            deny_open: false,
            deny_set: false,
        });
    }
    fn kill(&mut self, pid: u32) {
        self.procs.retain(|p| p.pid != pid);
    }
    fn proc(&mut self, pid: u32) -> &mut FakeProc {
        self.procs.iter_mut().find(|p| p.pid == pid).expect("no such fake process")
    }
    fn by_handle(&mut self, handle: Handle) -> Option<&mut FakeProc> {
        self.procs.iter_mut().find(|p| p.pid as Handle == handle)
    }
    fn logged(&self, needle: &str) -> usize {
        self.logs.iter().filter(|l| l.contains(needle)).count()
    }
}

impl System for Fake {
    fn pids(&mut self, out: &mut Vec<u32>) {
        out.clear();
        out.extend(self.procs.iter().map(|p| p.pid));
    }
    fn info(&mut self, pid: u32) -> Option<(String, u64)> {
        self.info_calls += 1;
        self.procs.iter().find(|p| p.pid == pid).map(|p| (p.path.clone(), p.start))
    }
    fn open(&mut self, pid: u32) -> Result<Handle, u32> {
        if self.proc(pid).deny_open {
            return Err(ACCESS_DENIED);
        }
        self.open_handles.push(pid as Handle);
        Ok(pid as Handle)
    }
    fn close(&mut self, handle: Handle) {
        let i =
            self.open_handles.iter().position(|&h| h == handle).expect("closing unknown handle");
        self.open_handles.swap_remove(i);
    }
    fn exited(&mut self, handle: Handle) -> bool {
        self.by_handle(handle).is_none()
    }
    fn set_affinity(&mut self, handle: Handle, mask: u64) -> Result<(), u32> {
        match self.by_handle(handle) {
            Some(p) if !p.deny_set => {
                p.affinity = mask;
                Ok(())
            }
            _ => Err(ACCESS_DENIED),
        }
    }
    fn affinity(&mut self, handle: Handle) -> Option<u64> {
        self.by_handle(handle).map(|p| p.affinity)
    }
    fn set_cpu_sets(&mut self, handle: Handle, ids: &[u32]) -> Result<(), u32> {
        match self.by_handle(handle) {
            Some(p) if !p.deny_set => {
                p.sets = ids.to_vec();
                Ok(())
            }
            _ => Err(ACCESS_DENIED),
        }
    }
    fn cpu_sets(&mut self, handle: Handle) -> Option<Vec<u32>> {
        self.by_handle(handle).map(|p| p.sets.clone())
    }
    fn log(&mut self, msg: &str) {
        self.logs.push(msg.to_string());
    }
}

/// A 9950X3D-like topology: 32 logical cpus, V-Cache on 0-15.
fn topo() -> Topology {
    Topology {
        all_mask: ALL,
        l3: vec![L3 { size: 96 << 20, mask: CACHE }, L3 { size: 32 << 20, mask: FREQ }],
        physical_cores: 16,
        cpu_sets: (0..32u8).map(|i| (i, 0x100 + i as u32)).collect(),
    }
}

fn cache_sets() -> Vec<u32> {
    (0x100..0x110).collect()
}

fn rule(pattern: &str, profile: &str) -> Rule {
    Rule { pattern: pattern.into(), profile: profile.into(), mode: Mode::Affinity, enforce: true }
}

/// Temp config directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pinfold-unit-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Dir(dir)
    }
    fn write(&self, rules: Vec<Rule>) {
        let cfg = Config {
            poll_interval_ms: 1000,
            elevated_autostart: true,
            profiles: vec![
                Profile { name: "Cache".into(), cpus: "0-15".into() },
                Profile { name: "Frequency".into(), cpus: "16-31".into() },
                Profile { name: "All cores".into(), cpus: "0-31".into() },
            ],
            rules,
        };
        self.write_with(|| config::save(&self.0, &cfg).unwrap());
    }
    fn write_raw(&self, text: &str) {
        std::fs::create_dir_all(&self.0).unwrap();
        self.write_with(|| std::fs::write(config::path(&self.0), text).unwrap());
    }
    /// File times have coarse resolution: repeat the write until the
    /// modification time differs from the previous one, as a real edit would.
    fn write_with(&self, write: impl Fn()) {
        let before = config::mtime(&self.0);
        loop {
            write();
            if config::mtime(&self.0) != before {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine(dir: &Dir, rules: Vec<Rule>) -> Engine<Fake> {
    dir.write(rules);
    Engine::new(Fake::default(), dir.0.clone(), topo())
}

// --- pure helpers -----------------------------------------------------------

#[test]
fn matching_prefers_full_path_then_first_rule() {
    let c = Config {
        poll_interval_ms: 1000,
        elevated_autostart: true,
        profiles: vec![],
        rules: vec![
            rule("foo.exe", "Cache"),
            rule("D:\\Games\\Foo\\foo.exe", "Cache"),
            rule("bar.exe", "Cache"),
        ],
    };
    assert_eq!(match_rule(&c, "d:\\games\\foo\\FOO.exe", |_| true), Some(1));
    assert_eq!(match_rule(&c, "C:\\other\\Foo.EXE", |_| true), Some(0));
    assert_eq!(match_rule(&c, "C:\\x\\bar.exe", |_| true), Some(2));
    assert_eq!(match_rule(&c, "C:\\x\\foobar.exe", |_| true), None);
    assert_eq!(match_rule(&c, "d:\\games\\foo\\foo.exe", |i| i != 1), Some(0));
}

#[test]
fn back_off_counts_only_drifts_inside_the_window() {
    let t0 = Instant::now();
    let mut fast = VecDeque::new();
    for i in 0..DRIFT_LIMIT {
        assert!(!record_drift(&mut fast, t0 + Duration::from_secs(i as u64)));
    }
    assert!(record_drift(&mut fast, t0 + Duration::from_secs(10)));

    let mut slow = VecDeque::new();
    for i in 0..20 {
        assert!(!record_drift(&mut slow, t0 + Duration::from_secs(i * 15)));
    }
}

// --- first run and config ---------------------------------------------------

#[test]
fn first_run_creates_config_with_topology_profiles() {
    let dir = Dir::new();
    let e = Engine::new(Fake::default(), dir.0.clone(), topo());
    assert!(config::path(&dir.0).exists());
    let names: Vec<&str> = e.cfg.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Cache", "Frequency", "All cores"]);
    assert_eq!(e.cfg.profiles[0].cpus, "0-15");
    assert_eq!(e.sys.logged("created"), 1);
}

#[test]
fn broken_config_at_startup_is_not_overwritten() {
    let dir = Dir::new();
    dir.write_raw("{ not json");
    let e = Engine::new(Fake::default(), dir.0.clone(), topo());
    assert!(e.cfg.rules.is_empty());
    assert_eq!(e.sys.logged("config error"), 1);
    assert_eq!(std::fs::read_to_string(config::path(&dir.0)).unwrap(), "{ not json");
}

#[test]
fn rule_with_unknown_profile_is_ignored_and_logged() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Nope"), rule("tool.exe", "Cache")]);
    assert_eq!(e.sys.logged("rule 'game.exe' ignored"), 1);
    e.sys.spawn(10, "C:\\g\\game.exe");
    e.sys.spawn(11, "C:\\t\\tool.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, ALL);
    assert_eq!(e.sys.proc(11).affinity, CACHE);
}

// --- applying ----------------------------------------------------------------

#[test]
fn applies_to_matching_new_process_only() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\Games\\Game.exe");
    e.sys.spawn(11, "C:\\Windows\\other.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    assert_eq!(e.sys.proc(11).affinity, ALL);
    assert_eq!(e.tracked.len(), 1);
    assert_eq!(e.rule_status(0), (State::Ok, 1));
    assert_eq!(e.sys.logged("applied Game.exe pid 10 -> Cache (cpus 0-15) mode=affinity"), 1);
}

#[test]
fn process_started_later_is_pinned_on_the_next_poll() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Frequency")]);
    e.tick();
    e.sys.spawn(20, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(20).affinity, FREQ);
}

#[test]
fn only_new_pids_are_inspected() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    for pid in 1..=50 {
        e.sys.spawn(pid, "C:\\Windows\\svc.exe");
    }
    e.tick();
    assert_eq!(e.sys.info_calls, 50);
    e.tick();
    e.tick();
    assert_eq!(e.sys.info_calls, 50, "known PIDs must not be queried again");
    e.sys.spawn(51, "C:\\new.exe");
    e.tick();
    assert_eq!(e.sys.info_calls, 51);
}

#[test]
fn mode_cpusets_leaves_the_affinity_mask_alone() {
    let dir = Dir::new();
    let mut r = rule("game.exe", "Cache");
    r.mode = Mode::Cpusets;
    let mut e = engine(&dir, vec![r]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, ALL);
    assert_eq!(e.sys.proc(10).sets, cache_sets());
}

#[test]
fn mode_both_sets_mask_and_cpu_sets() {
    let dir = Dir::new();
    let mut r = rule("game.exe", "Cache");
    r.mode = Mode::Both;
    let mut e = engine(&dir, vec![r]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    assert_eq!(e.sys.proc(10).sets, cache_sets());
}

#[test]
fn profile_is_clipped_to_the_cpus_that_exist() {
    let dir = Dir::new();
    dir.write(vec![rule("game.exe", "Cache")]);
    let mut small = topo();
    small.all_mask = 0xFF; // only 8 logical cpus
    let mut e = Engine::new(Fake::default(), dir.0.clone(), small);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, 0xFF);
}

// --- drift and back-off -----------------------------------------------------

#[test]
fn drift_is_restored_and_logged() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    e.sys.proc(10).affinity = FREQ;
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    assert_eq!(e.sys.logged("drift game.exe pid 10: expected cpus 0-15, found affinity 16-31"), 1);
    e.tick();
    assert_eq!(e.sys.logged("drift"), 1, "no drift, no log entry");
}

#[test]
fn cpu_set_drift_is_detected_in_mode_both() {
    let dir = Dir::new();
    let mut r = rule("game.exe", "Cache");
    r.mode = Mode::Both;
    let mut e = engine(&dir, vec![r]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    e.sys.proc(10).sets.clear();
    e.tick();
    assert_eq!(e.sys.proc(10).sets, cache_sets());
    assert_eq!(e.sys.logged("found cpusets 0 ids"), 1);
}

#[test]
fn sixth_drift_in_a_minute_marks_contested_and_stops_reapplying() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    for _ in 0..DRIFT_LIMIT {
        e.sys.proc(10).affinity = FREQ;
        e.tick();
        assert_eq!(e.sys.proc(10).affinity, CACHE);
    }
    assert_eq!(e.rule_status(0), (State::Ok, 1));

    e.sys.proc(10).affinity = FREQ;
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, FREQ, "contested: left alone");
    assert_eq!(e.rule_status(0), (State::Contested, 1));
    e.tick();
    e.tick();
    assert_eq!(e.sys.logged("contested"), 1, "logged once");
    assert_eq!(e.sys.logged("drift game.exe"), DRIFT_LIMIT);
}

#[test]
fn contested_applies_to_the_instance_not_the_rule() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    for _ in 0..=DRIFT_LIMIT {
        e.sys.proc(10).affinity = FREQ;
        e.tick();
    }
    assert_eq!(e.rule_status(0).0, State::Contested);
    e.sys.kill(10);
    e.sys.spawn(11, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(11).affinity, CACHE);
    assert_eq!(e.rule_status(0), (State::Ok, 1));
}

#[test]
fn enforce_off_applies_once_and_never_reapplies() {
    let dir = Dir::new();
    let mut r = rule("game.exe", "Cache");
    r.enforce = false;
    let mut e = engine(&dir, vec![r]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    e.sys.proc(10).affinity = FREQ;
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, FREQ);
    assert_eq!(e.sys.logged("drift"), 0);
}

// --- denied ------------------------------------------------------------------

#[test]
fn process_that_refuses_the_handle_is_denied_and_logged_once() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.sys.proc(10).deny_open = true;
    for _ in 0..5 {
        e.tick();
    }
    assert_eq!(e.rule_status(0), (State::Denied, 1));
    assert_eq!(e.sys.logged("denied game.exe pid 10"), 1);
    assert_eq!(e.sys.proc(10).affinity, ALL);
}

#[test]
fn failed_set_is_denied_and_the_handle_is_closed() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.sys.proc(10).deny_set = true;
    e.tick();
    assert_eq!(e.rule_status(0), (State::Denied, 1));
    assert_eq!(e.sys.logged("cannot set affinity (error 5)"), 1);
    assert!(e.sys.open_handles.is_empty());
}

#[test]
fn denied_process_does_not_block_others() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.sys.proc(10).deny_open = true;
    e.sys.spawn(11, "E:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(11).affinity, CACHE);
    assert_eq!(e.rule_status(0), (State::Denied, 2));
}

// --- process lifetime --------------------------------------------------------

#[test]
fn exited_process_is_forgotten_and_its_handle_closed() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.open_handles, [10]);
    e.sys.kill(10);
    e.tick();
    assert!(e.tracked.is_empty());
    assert!(e.sys.open_handles.is_empty());
    assert_eq!(e.rule_status(0), (State::Ok, 0));
}

#[test]
fn restarted_program_is_pinned_again() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    e.sys.kill(10);
    e.tick();
    e.sys.spawn(12, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(12).affinity, CACHE);
    assert_eq!(e.sys.logged("applied"), 2);
}

#[test]
fn reused_pid_is_matched_as_a_new_process() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "C:\\other.exe");
    e.tick();
    e.sys.kill(10);
    e.tick();
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
}

#[test]
fn pid_of_a_denied_process_reused_within_one_poll_is_noticed() {
    // A denied process has no open handle, so nothing stops Windows from
    // giving its PID to a new process between two polls (F6).
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.sys.proc(10).deny_open = true;
    e.tick();
    assert_eq!(e.rule_status(0), (State::Denied, 1));

    e.sys.kill(10);
    e.sys.spawn(10, "D:\\game.exe"); // same PID, new process, can be opened
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    assert_eq!(e.rule_status(0), (State::Ok, 1));
}

#[test]
fn pid_of_a_denied_process_reused_by_another_program_clears_the_state() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.sys.proc(10).deny_open = true;
    e.tick();
    e.sys.kill(10);
    e.sys.spawn(10, "C:\\Windows\\other.exe");
    e.tick();
    assert_eq!(e.rule_status(0), (State::Ok, 0), "the rule must not stay 'denied'");
    assert_eq!(e.sys.proc(10).affinity, ALL);
}

#[test]
fn denied_process_that_keeps_running_is_not_reinspected() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.sys.proc(10).deny_open = true;
    for _ in 0..4 {
        e.tick();
    }
    assert_eq!(e.sys.logged("denied"), 1);
    assert_eq!(e.tracked.len(), 1);
}

// --- config changes ----------------------------------------------------------

#[test]
fn config_edit_on_disk_is_picked_up() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    dir.write(vec![rule("game.exe", "Frequency")]);
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, FREQ);
    assert_eq!(e.sys.logged("config reloaded: 1 rule(s)"), 1);
}

#[test]
fn removing_a_rule_resets_running_processes_to_all_cores() {
    let dir = Dir::new();
    let mut r = rule("game.exe", "Cache");
    r.mode = Mode::Both;
    let mut e = engine(&dir, vec![r]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    dir.write(vec![]);
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, ALL);
    assert!(e.sys.proc(10).sets.is_empty());
    assert!(e.tracked.is_empty());
    assert!(e.sys.open_handles.is_empty());
    assert_eq!(e.sys.logged("reset game.exe pid 10 to all cores"), 1);
}

#[test]
fn switching_mode_from_both_to_affinity_clears_cpu_sets() {
    let dir = Dir::new();
    let mut r = rule("game.exe", "Cache");
    r.mode = Mode::Both;
    let mut e = engine(&dir, vec![r]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    dir.write(vec![rule("game.exe", "Cache")]);
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    assert!(e.sys.proc(10).sets.is_empty());
}

#[test]
fn broken_config_edit_keeps_the_previous_rules() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    dir.write_raw("{ \"rules\": [ oops");
    e.tick();
    e.tick();
    assert_eq!(e.sys.logged("keeping previous config"), 1, "logged once per change");
    assert_eq!(e.cfg.rules.len(), 1);
    e.sys.proc(10).affinity = FREQ;
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, CACHE, "still enforcing the old rule");
}

#[test]
fn editing_the_config_clears_the_contested_state() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    for _ in 0..=DRIFT_LIMIT {
        e.sys.proc(10).affinity = FREQ;
        e.tick();
    }
    assert_eq!(e.rule_status(0).0, State::Contested);
    let mut r = rule("game.exe", "Cache");
    r.mode = Mode::Both;
    dir.write(vec![r]);
    e.tick();
    assert_eq!(e.rule_status(0), (State::Ok, 1));
    assert_eq!(e.sys.proc(10).affinity, CACHE);
}

#[test]
fn save_and_apply_persists_and_applies_immediately() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    e.cfg.upsert_rule("game.exe", "Cache", None);
    e.save_and_apply();
    assert_eq!(e.sys.proc(10).affinity, CACHE, "F1: applied without waiting for a poll");
    assert_eq!(config::load(&dir.0).unwrap().rules.len(), 1);
    e.tick();
    assert_eq!(e.sys.logged("config reloaded"), 0, "own write is not treated as an external edit");
}

#[test]
fn ui_edit_over_a_broken_config_keeps_a_backup() {
    let dir = Dir::new();
    dir.write_raw("{ my half-finished edit");
    let mut e = Engine::new(Fake::default(), dir.0.clone(), topo());
    e.cfg.upsert_rule("game.exe", "Cache", None);
    e.save_and_apply();
    let backup = std::fs::read_to_string(dir.0.join("config.broken.json")).unwrap();
    assert_eq!(backup, "{ my half-finished edit");
    assert_eq!(config::load(&dir.0).unwrap().rules.len(), 1);
}

// --- pause --------------------------------------------------------------------

#[test]
fn pause_stops_applying_and_enforcing_and_resume_catches_up() {
    let dir = Dir::new();
    let mut e = engine(&dir, vec![rule("game.exe", "Cache")]);
    e.sys.spawn(10, "D:\\game.exe");
    e.tick();
    e.set_paused(true);
    e.sys.proc(10).affinity = FREQ;
    e.sys.spawn(11, "E:\\game.exe");
    e.tick();
    assert_eq!(e.sys.proc(10).affinity, FREQ);
    assert_eq!(e.sys.proc(11).affinity, ALL);

    e.set_paused(false);
    assert_eq!(e.sys.proc(10).affinity, CACHE);
    assert_eq!(e.sys.proc(11).affinity, CACHE);
    assert_eq!(e.sys.open_handles.len(), 2, "no handles leaked across pause");
    assert_eq!(e.sys.logged("paused"), 1);
    assert_eq!(e.sys.logged("resumed"), 1);
}
