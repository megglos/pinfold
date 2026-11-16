//! "Start with Windows".
//!
//! Elevated variant (default): a Task Scheduler logon task with the highest
//! rights. Pinfold can then pin elevated games without a UAC prompt at each
//! logon. To make or delete the task, the user gets one UAC prompt.
//!
//! Variant without elevation: a value in `HKCU\...\Run`. To use it, set
//! `"elevatedAutostart": false` in the config.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW};

use crate::win::wide;

pub const TASK_NAME: &str = "Pinfold";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Pinfold";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x40;
const ERROR_CANCELLED: u32 = 1223;
const INFINITE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Off,
    /// `HKCU\...\Run`
    User,
    /// Task Scheduler logon task, highest rights
    Elevated,
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Off => "off",
            Kind::User => "on (unelevated, HKCU Run)",
            Kind::Elevated => "on (elevated, Task Scheduler logon task)",
        }
    }
}

/// Always the copy in System32, not one from PATH: this program runs
/// elevated.
fn schtasks() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("schtasks.exe")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Task definition: run `exe` at logon of `user` with the highest rights of
/// that user. Without the defaults that stop a background tool (battery,
/// 3-day time limit).
pub fn task_xml(user: &str, exe: &str) -> String {
    let user = xml_escape(user);
    let exe = xml_escape(exe);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Pinfold: persistent per-process CPU affinity. Starts the tray tool at logon.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn current_user() -> String {
    let name = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{name}"),
        _ => name,
    }
}

/// Runs schtasks without elevation and without a console window.
fn schtasks_quiet(args: &[&str]) -> bool {
    Command::new(schtasks())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn task_exists() -> bool {
    schtasks_quiet(&["/Query", "/TN", TASK_NAME])
}

/// Starts the logon task now. Works without elevation.
pub fn run_task() -> bool {
    schtasks_quiet(&["/Run", "/TN", TASK_NAME])
}

/// Runs schtasks elevated and waits. Shows a UAC prompt if not elevated.
fn schtasks_elevated(params: &str) -> Result<(), String> {
    unsafe {
        let file = wide(&schtasks().to_string_lossy());
        let verb = wide("runas");
        let params = wide(params);
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = 0; // SW_HIDE
        if ShellExecuteExW(&mut info) == 0 {
            let err = GetLastError();
            return Err(if err == ERROR_CANCELLED {
                "The elevation prompt was cancelled; nothing was changed.".to_string()
            } else {
                format!("Could not start the Task Scheduler tool (error {err}).")
            });
        }
        if info.hProcess.is_null() {
            return Err("Could not start the Task Scheduler tool.".to_string());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if code == 0 {
            Ok(())
        } else {
            Err(format!("The Task Scheduler tool failed (exit code {code})."))
        }
    }
}

fn create_task() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let xml = task_xml(&current_user(), &exe.to_string_lossy());
    // schtasks expects UTF-16 with a byte order mark.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let file = std::env::temp_dir().join(format!("pinfold-task-{}.xml", std::process::id()));
    std::fs::write(&file, bytes).map_err(|e| format!("Cannot write the task definition: {e}"))?;
    let result =
        schtasks_elevated(&format!("/Create /TN \"{TASK_NAME}\" /XML \"{}\" /F", file.display()));
    let _ = std::fs::remove_file(&file);
    result
}

fn delete_task() -> Result<(), String> {
    schtasks_elevated(&format!("/Delete /TN \"{TASK_NAME}\" /F"))
}

// --- HKCU Run -------------------------------------------------------------------

fn run_key(access: u32) -> Option<HKEY> {
    let mut h: HKEY = null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, access, &mut h) };
    (rc == 0).then_some(h)
}

fn run_value_exists() -> bool {
    let Some(h) = run_key(KEY_READ) else {
        return false;
    };
    let rc = unsafe {
        RegQueryValueExW(h, wide(RUN_VALUE).as_ptr(), null(), null_mut(), null_mut(), null_mut())
    };
    unsafe { RegCloseKey(h) };
    rc == 0
}

fn set_run_value(on: bool) -> Result<(), String> {
    let Some(h) = run_key(KEY_SET_VALUE) else {
        return Err("Cannot open the Run registry key.".to_string());
    };
    let rc = unsafe {
        let rc = if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = wide(&format!("\"{}\"", exe.display()));
            RegSetValueExW(
                h,
                wide(RUN_VALUE).as_ptr(),
                0,
                1, // REG_SZ
                cmd.as_ptr() as *const u8,
                (cmd.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(h, wide(RUN_VALUE).as_ptr())
        };
        RegCloseKey(h);
        rc
    };
    if rc == 0 || !on {
        Ok(())
    } else {
        Err(format!("Cannot write the Run registry value (error {rc})."))
    }
}

// --- Public operations ----------------------------------------------------------

pub fn status() -> Kind {
    if task_exists() {
        Kind::Elevated
    } else if run_value_exists() {
        Kind::User
    } else {
        Kind::Off
    }
}

/// Set autostart on in the given variant. Remove the other variant, thus
/// Pinfold does not start two times.
pub fn enable(elevated: bool) -> Result<Kind, String> {
    if elevated {
        create_task()?;
        let _ = set_run_value(false);
        Ok(Kind::Elevated)
    } else {
        if task_exists() {
            delete_task()?;
        }
        set_run_value(true)?;
        Ok(Kind::User)
    }
}

pub fn disable() -> Result<Kind, String> {
    if task_exists() {
        delete_task()?;
    }
    set_run_value(false)?;
    Ok(Kind::Off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_at_logon_with_highest_privileges_and_no_time_limit() {
        let xml = task_xml("DESK\\alice", "C:\\Programs\\Pinfold\\Pinfold.exe");
        assert!(xml.contains("<LogonTrigger>"));
        assert_eq!(xml.matches("<UserId>DESK\\alice</UserId>").count(), 2);
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<Command>C:\\Programs\\Pinfold\\Pinfold.exe</Command>"));
    }

    #[test]
    fn special_characters_are_escaped() {
        let xml = task_xml("R&D\\o<k>", "C:\\Tom & Jerry\\\"x\".exe");
        assert!(xml.contains("<UserId>R&amp;D\\o&lt;k&gt;</UserId>"));
        assert!(xml.contains("<Command>C:\\Tom &amp; Jerry\\&quot;x&quot;.exe</Command>"));
        assert!(!xml.contains("Tom & Jerry"));
    }

    #[test]
    fn schtasks_is_taken_from_system32() {
        let p = schtasks().to_string_lossy().to_lowercase();
        assert!(p.ends_with("\\system32\\schtasks.exe"), "{p}");
    }
}
