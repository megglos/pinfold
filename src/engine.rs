//! Poll, match, apply, drift check, back-off.
//!
//! The engine has no UI and no direct OS dependencies: everything it does
//! to processes goes through the `System` trait, so it can be tested with
//! a fake (see the tests at the bottom). `sys::RealSystem` is the real one.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::config::{self, Config, Mode};
use crate::topology::{self, format_mask, Topology};
use crate::win::file_name;

/// More than this many drifts inside the window marks the instance contested.
pub const DRIFT_LIMIT: usize = 5;
pub const DRIFT_WINDOW: Duration = Duration::from_secs(60);

/// Opaque process handle owned by the `System` implementation.
pub type Handle = usize;

/// Everything the engine needs from the operating system.
/// Errors are Win32 error codes.
pub trait System {
    /// Replace `out` with the PIDs of all running processes.
    fn pids(&mut self, out: &mut Vec<u32>);
    /// Image path and start time; None when the process cannot be queried.
    fn info(&mut self, pid: u32) -> Option<(String, u64)>;
    /// Open a process for reading and changing its affinity.
    fn open(&mut self, pid: u32) -> Result<Handle, u32>;
    fn close(&mut self, handle: Handle);
    fn exited(&mut self, handle: Handle) -> bool;
    fn set_affinity(&mut self, handle: Handle, mask: u64) -> Result<(), u32>;
    fn affinity(&mut self, handle: Handle) -> Option<u64>;
    /// An empty slice clears the process default CPU Sets.
    fn set_cpu_sets(&mut self, handle: Handle, ids: &[u32]) -> Result<(), u32>;
    fn cpu_sets(&mut self, handle: Handle) -> Option<Vec<u32>>;
    fn log(&mut self, msg: &str);
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Ok,
    Contested,
    Denied,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Contested => "contested",
            State::Denied => "denied",
        }
    }
}

/// A running process that matched a rule, identified by PID plus start
/// time. The handle stays open for enforcement, which also keeps the PID
/// from being reused; a denied process has no handle, so its start time is
/// compared on every poll instead.
pub struct Tracked {
    pub pid: u32,
    start: u64,
    handle: Option<Handle>,
    pub path: String,
    pub rule: usize,
    pub state: State,
    last_apply: Instant,
    drifts: VecDeque<Instant>,
}

struct Resolved {
    mask: u64,
    sets: Vec<u32>,
}

pub struct Engine<S: System> {
    pub sys: S,
    dir: PathBuf,
    pub cfg: Config,
    pub topo: Topology,
    /// Per rule: the resolved profile, or None when the rule is invalid.
    resolved: Vec<Option<Resolved>>,
    cfg_mtime: Option<SystemTime>,
    /// Sorted PID list of the previous poll.
    prev: Vec<u32>,
    cur: Vec<u32>,
    pub tracked: Vec<Tracked>,
    pub paused: bool,
}

/// Full path rules win over file name rules; within each kind the first
/// rule in the file wins. `usable` filters out invalid rules.
pub fn match_rule(cfg: &Config, path: &str, usable: impl Fn(usize) -> bool) -> Option<usize> {
    let path_l = path.to_lowercase();
    let name_l = file_name(&path_l);
    let is_path = |p: &str| p.contains('\\') || p.contains('/');
    let by_path = cfg.rules.iter().enumerate().position(|(i, r)| {
        usable(i) && is_path(&r.pattern) && r.pattern.to_lowercase().replace('/', "\\") == path_l
    });
    by_path.or_else(|| {
        cfg.rules.iter().enumerate().position(|(i, r)| {
            usable(i) && !is_path(&r.pattern) && r.pattern.to_lowercase() == name_l
        })
    })
}

/// Records one drift and reports whether the instance is now contested.
fn record_drift(drifts: &mut VecDeque<Instant>, now: Instant) -> bool {
    drifts.push_back(now);
    while let Some(&front) = drifts.front() {
        if now.duration_since(front) > DRIFT_WINDOW {
            drifts.pop_front();
        } else {
            break;
        }
    }
    drifts.len() > DRIFT_LIMIT
}

impl<S: System> Engine<S> {
    /// `dir` holds config.json; it is created with defaults on first run.
    pub fn new(mut sys: S, dir: PathBuf, topo: Topology) -> Engine<S> {
        let (cfg, note) = config::load_or_create(&dir, &topo);
        if let Some(note) = note {
            sys.log(&note);
        }
        let cfg_mtime = config::mtime(&dir);
        let mut e = Engine {
            sys,
            dir,
            cfg,
            topo,
            resolved: Vec::new(),
            cfg_mtime,
            prev: Vec::new(),
            cur: Vec::new(),
            tracked: Vec::new(),
            paused: false,
        };
        e.resolve();
        e
    }

    fn resolve(&mut self) {
        let mut resolved = Vec::with_capacity(self.cfg.rules.len());
        for r in &self.cfg.rules {
            let entry = match self.cfg.profile(&r.profile) {
                None => Err(format!("unknown profile '{}'", r.profile)),
                Some(p) => topology::parse_cpus(&p.cpus).and_then(|m| {
                    let mask = m & self.topo.all_mask;
                    if mask == 0 {
                        Err(format!("profile '{}' has no cpus on this system", p.name))
                    } else {
                        Ok(Resolved { mask, sets: self.topo.cpu_set_ids(mask) })
                    }
                }),
            };
            match entry {
                Ok(r) => resolved.push(Some(r)),
                Err(e) => {
                    self.sys.log(&format!("config error: rule '{}' ignored: {e}", r.pattern));
                    resolved.push(None);
                }
            }
        }
        self.resolved = resolved;
    }

    fn find_rule(&self, path: &str) -> Option<usize> {
        match_rule(&self.cfg, path, |i| self.resolved[i].is_some())
    }

    /// One poll. Cheap when nothing changed: one PID list, a diff, and a
    /// few calls per matched process.
    pub fn tick(&mut self) {
        if config::mtime(&self.dir) != self.cfg_mtime {
            self.reload_from_disk();
        }
        if self.paused {
            return;
        }
        self.cur.clear();
        self.sys.pids(&mut self.cur);
        self.cur.sort_unstable();

        // Drop processes that exited.
        let mut i = 0;
        while i < self.tracked.len() {
            let t = &self.tracked[i];
            let (pid, start) = (t.pid, t.start);
            let gone = self.cur.binary_search(&pid).is_err()
                || match t.handle {
                    Some(h) => self.sys.exited(h),
                    // No handle pins the PID: it may belong to a new process.
                    None => self.sys.info(pid).is_some_and(|(_, s)| s != start),
                };
            if gone {
                let t = self.tracked.swap_remove(i);
                if let Some(h) = t.handle {
                    self.sys.close(h);
                }
                // A reused PID is still in the list: look at it again.
                if let Ok(pos) = self.prev.binary_search(&pid) {
                    self.prev.remove(pos);
                }
            } else {
                i += 1;
            }
        }

        // Only new PIDs are opened and matched.
        for i in 0..self.cur.len() {
            let pid = self.cur[i];
            if pid == 0 || self.prev.binary_search(&pid).is_ok() {
                continue;
            }
            if self.tracked.iter().any(|t| t.pid == pid) {
                continue;
            }
            self.inspect(pid);
        }
        std::mem::swap(&mut self.prev, &mut self.cur);

        for i in 0..self.tracked.len() {
            self.check(i);
        }
    }

    fn inspect(&mut self, pid: u32) {
        let Some((path, start)) = self.sys.info(pid) else {
            return;
        };
        let Some(rule) = self.find_rule(&path) else {
            return;
        };
        let name = file_name(&path).to_string();
        let mut t = Tracked {
            pid,
            start,
            handle: None,
            path,
            rule,
            state: State::Ok,
            last_apply: Instant::now(),
            drifts: VecDeque::new(),
        };
        match self.sys.open(pid) {
            Err(err) => {
                self.sys
                    .log(&format!("denied {name} pid {pid}: cannot open process (error {err})"));
                t.state = State::Denied;
            }
            Ok(handle) => match self.apply(handle, rule) {
                Ok(()) => {
                    t.handle = Some(handle);
                    let r = &self.cfg.rules[rule];
                    let mask = self.resolved[rule].as_ref().map(|r| r.mask).unwrap_or(0);
                    self.sys.log(&format!(
                        "applied {name} pid {pid} -> {} (cpus {}) mode={}",
                        r.profile,
                        format_mask(mask),
                        r.mode.as_str()
                    ));
                }
                Err(err) => {
                    self.sys.log(&format!(
                        "denied {name} pid {pid}: cannot set affinity (error {err})"
                    ));
                    self.sys.close(handle);
                    t.state = State::Denied;
                }
            },
        }
        self.tracked.push(t);
    }

    fn apply(&mut self, handle: Handle, rule: usize) -> Result<(), u32> {
        let Some(res) = self.resolved[rule].as_ref() else {
            return Ok(());
        };
        let mode = self.cfg.rules[rule].mode;
        if mode != Mode::Cpusets {
            self.sys.set_affinity(handle, res.mask)?;
        }
        if mode != Mode::Affinity {
            self.sys.set_cpu_sets(handle, &res.sets)?;
        }
        Ok(())
    }

    /// Describes what was found when it differs from what the rule applied.
    fn mismatch(&mut self, handle: Handle, rule: usize) -> Option<String> {
        let res = self.resolved[rule].as_ref()?;
        let mode = self.cfg.rules[rule].mode;
        if mode != Mode::Cpusets {
            if let Some(found) = self.sys.affinity(handle) {
                if found != res.mask {
                    return Some(format!("affinity {}", format_mask(found)));
                }
            }
        }
        if mode != Mode::Affinity {
            if let Some(mut found) = self.sys.cpu_sets(handle) {
                found.sort_unstable();
                if found != res.sets {
                    return Some(format!("cpusets {} ids", found.len()));
                }
            }
        }
        None
    }

    fn check(&mut self, i: usize) {
        let t = &self.tracked[i];
        if t.state != State::Ok || !self.cfg.rules[t.rule].enforce {
            return;
        }
        let Some(handle) = t.handle else { return };
        let (rule, pid) = (t.rule, t.pid);
        let name = file_name(&t.path).to_string();
        let since = t.last_apply.elapsed().as_secs_f32();
        let Some(found) = self.mismatch(handle, rule) else {
            return;
        };
        let expected =
            self.resolved[rule].as_ref().map(|r| format_mask(r.mask)).unwrap_or_default();
        let now = Instant::now();

        if record_drift(&mut self.tracked[i].drifts, now) {
            self.tracked[i].state = State::Contested;
            self.sys.log(&format!(
                "contested {name} pid {pid}: drifted more than {DRIFT_LIMIT} times in {}s, \
                 expected cpus {expected}, found {found}; no longer re-applying for this instance",
                DRIFT_WINDOW.as_secs()
            ));
            return;
        }
        let result = self.apply(handle, rule);
        self.tracked[i].last_apply = now;
        let outcome = match result {
            Ok(()) => "re-applied".to_string(),
            Err(err) => format!("re-apply failed (error {err})"),
        };
        self.sys.log(&format!(
            "drift {name} pid {pid}: expected cpus {expected}, found {found}, \
             {since:.1}s since last apply; {outcome}"
        ));
    }

    fn release(&mut self, t: &Tracked, reset: bool) {
        let Some(handle) = t.handle else { return };
        if reset {
            let _ = self.sys.set_affinity(handle, self.topo.all_mask);
            let _ = self.sys.set_cpu_sets(handle, &[]);
        }
        self.sys.close(handle);
    }

    /// Call after `self.cfg` changed. Everything tracked is reset to all
    /// cores and the next poll re-matches every process from scratch.
    fn config_changed(&mut self) {
        self.resolve();
        let tracked = std::mem::take(&mut self.tracked);
        for t in &tracked {
            if self.find_rule(&t.path).is_none() && t.handle.is_some() {
                self.sys.log(&format!(
                    "reset {} pid {} to all cores (rule removed)",
                    file_name(&t.path),
                    t.pid
                ));
            }
            self.release(t, true);
        }
        self.prev.clear();
    }

    fn reload_from_disk(&mut self) {
        self.cfg_mtime = config::mtime(&self.dir);
        match config::load(&self.dir) {
            Ok(cfg) => {
                self.cfg = cfg;
                self.sys.log(&format!("config reloaded: {} rule(s)", self.cfg.rules.len()));
                self.config_changed();
            }
            Err(e) => self.sys.log(&format!("config error: {e}; keeping previous config")),
        }
    }

    /// Persist `self.cfg` after an edit from the UI and apply it right away.
    pub fn save_and_apply(&mut self) {
        // Never silently replace a file the user broke while editing it.
        let path = config::path(&self.dir);
        if path.exists() && config::load(&self.dir).is_err() {
            let backup = self.dir.join("config.broken.json");
            if std::fs::copy(&path, &backup).is_ok() {
                self.sys.log(&format!("unreadable config kept as {}", backup.display()));
            }
        }
        if let Err(e) = config::save(&self.dir, &self.cfg) {
            self.sys.log(&format!("config error: cannot write config: {e}"));
        }
        self.cfg_mtime = config::mtime(&self.dir);
        self.config_changed();
        self.tick();
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        self.sys.log(if paused { "paused" } else { "resumed" });
        if !paused {
            // Forget everything and rescan; pins set before the pause stay.
            let tracked = std::mem::take(&mut self.tracked);
            for t in &tracked {
                self.release(t, false);
            }
            self.prev.clear();
            self.tick();
        }
    }

    /// Worst state and instance count of the running processes for a rule.
    pub fn rule_status(&self, rule: usize) -> (State, usize) {
        let mut state = State::Ok;
        let mut count = 0;
        for t in self.tracked.iter().filter(|t| t.rule == rule) {
            count += 1;
            if t.state == State::Denied || (t.state == State::Contested && state == State::Ok) {
                state = t.state;
            }
        }
        (state, count)
    }
}

#[cfg(test)]
mod tests;
