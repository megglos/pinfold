//! Shared helpers for the integration tests. They run the real
//! `Pinfold.exe` with `PINFOLD_DIR` pointing at a private directory, so the
//! user's own config and a running Pinfold are never touched.

#![allow(dead_code)]

use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::{
    GetProcessAffinityMask, GetProcessDefaultCpuSets, SetProcessAffinityMask,
};

pub const EXE: &str = env!("CARGO_BIN_EXE_Pinfold");

static NEXT: AtomicU32 = AtomicU32::new(0);

fn unique() -> String {
    format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A private config directory, removed on drop.
pub struct Sandbox {
    pub dir: PathBuf,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let dir = std::env::temp_dir().join(format!("pinfold-it-{}", unique()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Resolve 8.3 short names (common in CI temp paths) so full-path
        // rules compare against the same spelling Windows reports.
        let long = std::fs::canonicalize(&dir).unwrap();
        let text = long.to_string_lossy();
        let dir = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text));
        Sandbox { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(EXE);
        c.args(args).env("PINFOLD_DIR", &self.dir).stdin(Stdio::null());
        c
    }

    /// Run a short-lived command and collect its output.
    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("cannot run Pinfold.exe")
    }

    /// Start a command without waiting; stdout is collected.
    #[allow(dead_code)] // not every test binary uses it
    pub fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.command(args).stdout(Stdio::piped()).spawn().expect("cannot run Pinfold.exe")
    }

    /// Like `run`, but returns stdout and asserts the exit code.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(out.status.code(), Some(0), "`{args:?}` failed: {text}");
        text
    }

    /// Start the engine in console mode (`Pinfold.exe run`).
    pub fn start_engine(&self) -> Proc {
        Proc(self.command(&["run"]).stdout(Stdio::null()).spawn().expect("cannot start engine"))
    }

    /// Start the background instance with tray icon. Its single-instance
    /// lock and window are keyed by the config directory, so it runs next
    /// to an installed Pinfold without either noticing the other.
    pub fn start_tray(&self) -> Proc {
        Proc(self.command(&[]).stdout(Stdio::null()).spawn().expect("cannot start tray instance"))
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn config(&self) -> String {
        std::fs::read_to_string(self.config_path()).unwrap_or_default()
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("pinfold.log")).unwrap_or_default()
    }

    /// Write a config with a one-cpu profile "One" (cpu 0 exists on every
    /// machine, including small CI runners) and a fast poll.
    pub fn write_config(&self, rules_json: &str) {
        let text = format!(
            r#"{{
  "pollIntervalMs": 250,
  "profiles": [ {{ "name": "One", "cpus": "0" }} ],
  "rules": [ {rules_json} ]
}}"#
        );
        std::fs::write(self.config_path(), text).unwrap();
    }

    /// Copy of an idle program under a unique file name, so that rules in
    /// one test can never match a process of another test. The program is
    /// Pinfold itself, idling in console mode with an empty private config.
    pub fn target(&self) -> Target {
        let name = format!("pf-target-{}.exe", unique());
        let dir = self.dir.join("target");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(&name);
        std::fs::copy(EXE, &path).unwrap();
        Target { name, path, cfg_dir: dir.join("cfg") }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct Target {
    pub name: String,
    pub path: PathBuf,
    cfg_dir: PathBuf,
}

impl Target {
    pub fn start(&self) -> Proc {
        let child = Command::new(&self.path)
            .arg("run")
            .env("PINFOLD_DIR", &self.cfg_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("cannot start target process");
        Proc(child)
    }
}

/// A child process that is killed on drop.
pub struct Proc(pub Child);

impl Proc {
    fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0.as_raw_handle() as _
    }

    /// (process affinity mask, system affinity mask)
    pub fn masks(&self) -> (usize, usize) {
        let (mut process, mut system) = (0usize, 0usize);
        let ok = unsafe { GetProcessAffinityMask(self.handle(), &mut process, &mut system) };
        assert_ne!(ok, 0, "GetProcessAffinityMask failed");
        (process, system)
    }

    pub fn affinity(&self) -> usize {
        self.masks().0
    }

    pub fn system_mask(&self) -> usize {
        self.masks().1
    }

    pub fn set_affinity(&self, mask: usize) {
        let ok = unsafe { SetProcessAffinityMask(self.handle(), mask) };
        assert_ne!(ok, 0, "SetProcessAffinityMask failed");
    }

    /// Number of process default CPU Sets.
    pub fn cpu_set_count(&self) -> u32 {
        let mut ids = [0u32; 64];
        let mut count = 0u32;
        unsafe { GetProcessDefaultCpuSets(self.handle(), ids.as_mut_ptr(), 64, &mut count) };
        count
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Poll `cond` until it holds or the timeout expires.
pub fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub const TIMEOUT: Duration = Duration::from_secs(10);

/// True when the machine has more than one logical cpu; pinning to cpu 0
/// cannot be told apart from "all cpus" otherwise.
pub fn multi_cpu(p: &Proc) -> bool {
    p.system_mask().count_ones() > 1
}
