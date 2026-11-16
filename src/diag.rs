//! Startup environment snapshot. Read-only.

use std::ptr::{null, null_mut};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows_sys::Win32::UI::Shell::IsUserAnAdmin;

use crate::topology::Topology;
use crate::win::{from_wide, wide};

const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;

struct Key(HKEY);

impl Key {
    fn open(root: HKEY, path: &str) -> Option<Key> {
        let mut h: HKEY = null_mut();
        let rc = unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ, &mut h) };
        (rc == 0).then_some(Key(h))
    }

    fn dword(&self, name: &str) -> Option<u32> {
        let mut data = 0u32;
        let mut len = 4u32;
        let mut ty = 0u32;
        let rc = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                null(),
                &mut ty,
                &mut data as *mut u32 as *mut u8,
                &mut len,
            )
        };
        (rc == 0 && ty == REG_DWORD).then_some(data)
    }

    fn values(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let mut data = [0u8; 512];
            let mut data_len = data.len() as u32;
            let mut ty = 0u32;
            let rc = unsafe {
                RegEnumValueW(
                    self.0,
                    i,
                    name.as_mut_ptr(),
                    &mut name_len,
                    null(),
                    &mut ty,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc != 0 {
                break;
            }
            let name = from_wide(&name);
            let d = &data[..data_len as usize];
            let value = match ty {
                REG_DWORD if d.len() >= 4 => {
                    u32::from_le_bytes([d[0], d[1], d[2], d[3]]).to_string()
                }
                REG_SZ => {
                    let (pairs, _) = d.as_chunks::<2>();
                    let w: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
                    format!("\"{}\"", from_wide(&w))
                }
                _ => format!("<type {ty}, {} bytes>", d.len()),
            };
            out.push(format!("{name}={value}"));
        }
        out
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// True for an unelevated process of an administrator: the account has a
/// second, elevated token, so the logon task (highest privileges) would
/// run elevated. False for a standard user and for an elevated process.
pub fn has_elevated_twin() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevationType, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    const TOKEN_ELEVATION_TYPE_LIMITED: u32 = 3;
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut kind = 0u32;
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevationType,
            &mut kind as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && kind == TOKEN_ELEVATION_TYPE_LIMITED
    }
}

pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// Lines describing the environment (F19).
pub fn snapshot(topo: &Topology) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Pinfold {} started, pid {}, elevated={}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            is_elevated()
        ),
        topo.describe(),
    ];
    let services = "SYSTEM\\CurrentControlSet\\Services\\";
    for svc in ["amd3dvcache", "amd3dvcacheSvc"] {
        let present = Key::open(HKEY_LOCAL_MACHINE, &format!("{services}{svc}")).is_some();
        lines.push(format!("env: {svc} {}", if present { "present" } else { "not present" }));
    }
    match Key::open(HKEY_LOCAL_MACHINE, &format!("{services}amd3dvcache\\Preferences")) {
        Some(k) => lines.push(format!("env: amd3dvcache preferences: {}", k.values().join(", "))),
        None => lines.push("env: amd3dvcache preferences: key not present".into()),
    }
    // Game Mode is on unless AutoGameModeEnabled is explicitly 0.
    let game_mode = Key::open(HKEY_CURRENT_USER, "Software\\Microsoft\\GameBar")
        .and_then(|k| k.dword("AutoGameModeEnabled"));
    lines.push(format!(
        "env: Windows Game Mode {}",
        match game_mode {
            Some(0) => "off",
            Some(_) => "on",
            None => "on (default, not set)",
        }
    ));
    lines
}

pub fn log_startup(topo: &Topology) {
    for line in snapshot(topo) {
        crate::log::log(&line);
    }
}
