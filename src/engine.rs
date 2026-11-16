//! Poll, match, apply, drift check, back-off. No UI dependencies.

use std::collections::VecDeque;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
use windows_sys::Win32::System::Threading::{
    GetProcessAffinityMask, GetProcessDefaultCpuSets, GetProcessTimes, OpenProcess,
    QueryFullProcessImageNameW, SetProcessAffinityMask, SetProcessDefaultCpuSets,
    WaitForSingleObject,
};

use crate::config::{self, Config, Mode};
use crate::log::log;
use crate::topology::{self, format_mask, Topology};
use crate::win::{file_name, from_wide};

const PROCESS_SET_INFORMATION: u32 = 0x0200;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_SET_LIMITED_INFORMATION: u32 = 0x2000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;

/// More than this many drifts inside the window marks the instance contested.
pub const DRIFT_LIMIT: usize = 5;
pub const DRIFT_WINDOW: Duration = Duration::from_secs(60);

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

/// A running process that matched a rule. The handle stays open for
/// enforcement, which also keeps the PID from being reused.
pub struct Tracked {
    pub pid: u32,
    handle: HANDLE,
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

pub struct Engine {
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

/// Image path and creation time of a process.
pub fn process_info(pid: u32) -> Option<(String, u64)> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
        let mut times = [std::mem::zeroed::<windows_sys::Win32::Foundation::FILETIME>(); 4];
        let (c, rest) = times.split_at_mut(1);
        let (e, rest) = rest.split_at_mut(1);
        let (k, u) = rest.split_at_mut(1);
        GetProcessTimes(h, &mut c[0], &mut e[0], &mut k[0], &mut u[0]);
        CloseHandle(h);
        if !ok {
            return None;
        }
        let start = ((c[0].dwHighDateTime as u64) << 32) | c[0].dwLowDateTime as u64;
        Some((from_wide(&buf[..len as usize]), start))
    }
}

impl Engine {
    pub fn new() -> Engine {
        let topo = topology::read();
        let cfg = config::load_or_create(&topo);
        let mut e = Engine {
            cfg,
            topo,
            resolved: Vec::new(),
            cfg_mtime: config::mtime(),
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
                    log(&format!("config error: rule '{}' ignored: {e}", r.pattern));
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
        if config::mtime() != self.cfg_mtime {
            self.reload_from_disk();
        }
        if self.paused {
            return;
        }
        self.enum_pids();

        // Drop processes that exited.
        let cur = &self.cur;
        self.tracked.retain(|t| {
            let alive = cur.binary_search(&t.pid).is_ok()
                && (t.handle.is_null()
                    || unsafe { WaitForSingleObject(t.handle, 0) } != WAIT_OBJECT_0);
            if !alive && !t.handle.is_null() {
                unsafe { CloseHandle(t.handle) };
            }
            alive
        });

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

    fn enum_pids(&mut self) {
        if self.cur.len() < 1024 {
            self.cur.resize(1024, 0);
        }
        loop {
            let bytes = (self.cur.len() * 4) as u32;
            let mut needed = 0u32;
            let ok = unsafe { EnumProcesses(self.cur.as_mut_ptr(), bytes, &mut needed) };
            if ok == 0 {
                self.cur.clear();
                return;
            }
            if needed < bytes {
                self.cur.truncate(needed as usize / 4);
                break;
            }
            let n = self.cur.len() * 2;
            self.cur.resize(n, 0);
        }
        self.cur.sort_unstable();
    }

    fn inspect(&mut self, pid: u32) {
        let Some((path, _start)) = process_info(pid) else { return };
        let Some(rule) = self.find_rule(&path) else { return };
        let name = file_name(&path).to_string();
        let access = PROCESS_SET_INFORMATION
            | PROCESS_SET_LIMITED_INFORMATION
            | PROCESS_QUERY_LIMITED_INFORMATION
            | SYNCHRONIZE;
        let handle = unsafe { OpenProcess(access, 0, pid) };
        let mut t = Tracked {
            pid,
            handle,
            path,
            rule,
            state: State::Ok,
            last_apply: Instant::now(),
            drifts: VecDeque::new(),
        };
        if handle.is_null() {
            let err = unsafe { GetLastError() };
            log(&format!("denied {name} pid {pid}: cannot open process (error {err})"));
            t.state = State::Denied;
        } else {
            match self.apply(handle, rule) {
                Ok(()) => {
                    let r = &self.cfg.rules[rule];
                    let mask = self.resolved[rule].as_ref().map(|r| r.mask).unwrap_or(0);
                    log(&format!(
                        "applied {name} pid {pid} -> {} (cpus {}) mode={}",
                        r.profile,
                        format_mask(mask),
                        r.mode.as_str()
                    ));
                }
                Err(err) => {
                    log(&format!("denied {name} pid {pid}: cannot set affinity (error {err})"));
                    unsafe { CloseHandle(handle) };
                    t.handle = null_mut();
                    t.state = State::Denied;
                }
            }
        }
        self.tracked.push(t);
    }

    fn apply(&self, handle: HANDLE, rule: usize) -> Result<(), u32> {
        let Some(res) = self.resolved[rule].as_ref() else { return Ok(()) };
        let mode = self.cfg.rules[rule].mode;
        unsafe {
            if mode != Mode::Cpusets && SetProcessAffinityMask(handle, res.mask as usize) == 0 {
                return Err(GetLastError());
            }
            if mode != Mode::Affinity
                && SetProcessDefaultCpuSets(handle, res.sets.as_ptr(), res.sets.len() as u32) == 0
            {
                return Err(GetLastError());
            }
        }
        Ok(())
    }

    /// Describes what was found when it differs from what the rule applied.
    fn mismatch(&self, handle: HANDLE, rule: usize) -> Option<String> {
        let res = self.resolved[rule].as_ref()?;
        let mode = self.cfg.rules[rule].mode;
        unsafe {
            if mode != Mode::Cpusets {
                let (mut process, mut system) = (0usize, 0usize);
                if GetProcessAffinityMask(handle, &mut process, &mut system) != 0
                    && process as u64 != res.mask
                {
                    return Some(format!("affinity {}", format_mask(process as u64)));
                }
            }
            if mode != Mode::Affinity {
                let mut ids = [0u32; 64];
                let mut count = 0u32;
                if GetProcessDefaultCpuSets(handle, ids.as_mut_ptr(), 64, &mut count) != 0 {
                    let mut found = ids[..count as usize].to_vec();
                    found.sort_unstable();
                    if found != res.sets {
                        return Some(format!("cpusets {} ids", found.len()));
                    }
                }
            }
        }
        None
    }

    fn check(&mut self, i: usize) {
        let t = &self.tracked[i];
        if t.state != State::Ok || t.handle.is_null() || !self.cfg.rules[t.rule].enforce {
            return;
        }
        let Some(found) = self.mismatch(t.handle, t.rule) else { return };
        let (handle, rule, pid) = (t.handle, t.rule, t.pid);
        let name = file_name(&t.path).to_string();
        let since = t.last_apply.elapsed().as_secs_f32();
        let expected = self.resolved[rule].as_ref().map(|r| format_mask(r.mask)).unwrap_or_default();
        let now = Instant::now();

        if record_drift(&mut self.tracked[i].drifts, now) {
            self.tracked[i].state = State::Contested;
            log(&format!(
                "contested {name} pid {pid}: drifted more than {DRIFT_LIMIT} times in {}s, \
                 expected cpus {expected}, found {found}; no longer re-applying for this instance",
                DRIFT_WINDOW.as_secs()
            ));
            return;
        }
        let result = self.apply(handle, rule);
        self.tracked[i].last_apply = now;
        match result {
            Ok(()) => log(&format!(
                "drift {name} pid {pid}: expected cpus {expected}, found {found}, \
                 {since:.1}s since last apply; re-applied"
            )),
            Err(err) => log(&format!(
                "drift {name} pid {pid}: expected cpus {expected}, found {found}, \
                 {since:.1}s since last apply; re-apply failed (error {err})"
            )),
        }
    }

    fn release(&self, t: &Tracked, reset: bool) {
        if t.handle.is_null() {
            return;
        }
        unsafe {
            if reset {
                SetProcessAffinityMask(t.handle, self.topo.all_mask as usize);
                SetProcessDefaultCpuSets(t.handle, null(), 0);
            }
            CloseHandle(t.handle);
        }
    }

    /// Call after `self.cfg` changed. Everything tracked is reset to all
    /// cores and the next poll re-matches every process from scratch.
    fn config_changed(&mut self) {
        self.resolve();
        let tracked = std::mem::take(&mut self.tracked);
        for t in &tracked {
            if self.find_rule(&t.path).is_none() && !t.handle.is_null() {
                log(&format!("reset {} pid {} to all cores (rule removed)", file_name(&t.path), t.pid));
            }
            self.release(t, true);
        }
        self.prev.clear();
    }

    fn reload_from_disk(&mut self) {
        self.cfg_mtime = config::mtime();
        match config::load() {
            Ok(cfg) => {
                self.cfg = cfg;
                log(&format!("config reloaded: {} rule(s)", self.cfg.rules.len()));
                self.config_changed();
            }
            Err(e) => log(&format!("config error: {e}; keeping previous config")),
        }
    }

    /// Persist `self.cfg` after an edit from the UI and apply it right away.
    pub fn save_and_apply(&mut self) {
        if let Err(e) = config::save(&self.cfg) {
            log(&format!("config error: cannot write config: {e}"));
        }
        self.cfg_mtime = config::mtime();
        self.config_changed();
        self.tick();
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        log(if paused { "paused" } else { "resumed" });
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
mod tests {
    use super::*;
    use crate::config::Rule;

    fn cfg(patterns: &[&str]) -> Config {
        Config {
            poll_interval_ms: 1000,
            profiles: vec![],
            rules: patterns
                .iter()
                .map(|p| Rule {
                    pattern: p.to_string(),
                    profile: "Cache".into(),
                    mode: Mode::Affinity,
                    enforce: true,
                })
                .collect(),
        }
    }

    #[test]
    fn matching() {
        let c = cfg(&["foo.exe", "D:\\Games\\Foo\\foo.exe", "bar.exe"]);
        // Full path beats file name even though it is listed later.
        assert_eq!(match_rule(&c, "d:\\games\\foo\\FOO.exe", |_| true), Some(1));
        assert_eq!(match_rule(&c, "C:\\other\\Foo.EXE", |_| true), Some(0));
        assert_eq!(match_rule(&c, "C:\\x\\bar.exe", |_| true), Some(2));
        assert_eq!(match_rule(&c, "C:\\x\\foobar.exe", |_| true), None);
        // Unusable rules are skipped.
        assert_eq!(match_rule(&c, "d:\\games\\foo\\foo.exe", |i| i != 1), Some(0));
    }

    #[test]
    fn back_off_after_limit() {
        let mut d = VecDeque::new();
        let t0 = Instant::now();
        for i in 0..DRIFT_LIMIT {
            assert!(!record_drift(&mut d, t0 + Duration::from_secs(i as u64)));
        }
        // Sixth drift inside the window: contested.
        assert!(record_drift(&mut d, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn slow_drift_is_not_contested() {
        let mut d = VecDeque::new();
        let t0 = Instant::now();
        for i in 0..20 {
            assert!(!record_drift(&mut d, t0 + Duration::from_secs(i * 15)));
        }
    }
}
