//! The real operating system behind the engine's `System` trait.

use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FILETIME, HANDLE};
use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
use windows_sys::Win32::System::Threading::{
    GetProcessAffinityMask, GetProcessDefaultCpuSets, GetProcessTimes, OpenProcess,
    QueryFullProcessImageNameW, SetProcessAffinityMask, SetProcessDefaultCpuSets,
    WaitForSingleObject,
};

use crate::engine::{Handle, System};
use crate::win::{file_name, from_wide};

const PROCESS_SET_INFORMATION: u32 = 0x0200;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_SET_LIMITED_INFORMATION: u32 = 0x2000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;

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
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(h);
        if !ok {
            return None;
        }
        let start = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        Some((from_wide(&buf[..len as usize]), start))
    }
}

/// PIDs of all running processes with this executable file name.
pub fn pids_by_name(name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    RealSystem.pids(&mut pids);
    pids.into_iter()
        .filter(|&pid| pid != 0)
        .filter(|&pid| {
            process_info(pid).is_some_and(|(path, _)| file_name(&path).eq_ignore_ascii_case(name))
        })
        .collect()
}

/// Affinity mask of a process. Needs only query rights, thus it also works
/// on an elevated process from a process without elevation.
pub fn affinity_of(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let (mut process, mut system) = (0usize, 0usize);
        let ok = GetProcessAffinityMask(h, &mut process, &mut system);
        CloseHandle(h);
        (ok != 0).then_some(process as u64)
    }
}

pub struct RealSystem;

impl System for RealSystem {
    fn pids(&mut self, out: &mut Vec<u32>) {
        let mut cap = out.capacity().max(1024);
        loop {
            out.resize(cap, 0);
            let bytes = (cap * 4) as u32;
            let mut needed = 0u32;
            if unsafe { EnumProcesses(out.as_mut_ptr(), bytes, &mut needed) } == 0 {
                out.clear();
                return;
            }
            if needed < bytes {
                out.truncate(needed as usize / 4);
                return;
            }
            cap *= 2;
        }
    }

    fn info(&mut self, pid: u32) -> Option<(String, u64)> {
        process_info(pid)
    }

    fn open(&mut self, pid: u32) -> Result<Handle, u32> {
        let access = PROCESS_SET_INFORMATION
            | PROCESS_SET_LIMITED_INFORMATION
            | PROCESS_QUERY_LIMITED_INFORMATION
            | SYNCHRONIZE;
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() {
            Err(unsafe { GetLastError() })
        } else {
            Ok(h as Handle)
        }
    }

    fn close(&mut self, handle: Handle) {
        unsafe { CloseHandle(handle as HANDLE) };
    }

    fn exited(&mut self, handle: Handle) -> bool {
        unsafe { WaitForSingleObject(handle as HANDLE, 0) == WAIT_OBJECT_0 }
    }

    fn set_affinity(&mut self, handle: Handle, mask: u64) -> Result<(), u32> {
        unsafe {
            if SetProcessAffinityMask(handle as HANDLE, mask as usize) == 0 {
                return Err(GetLastError());
            }
        }
        Ok(())
    }

    fn affinity(&mut self, handle: Handle) -> Option<u64> {
        let (mut process, mut system) = (0usize, 0usize);
        let ok = unsafe { GetProcessAffinityMask(handle as HANDLE, &mut process, &mut system) };
        (ok != 0).then_some(process as u64)
    }

    fn set_cpu_sets(&mut self, handle: Handle, ids: &[u32]) -> Result<(), u32> {
        let ptr = if ids.is_empty() { null() } else { ids.as_ptr() };
        unsafe {
            if SetProcessDefaultCpuSets(handle as HANDLE, ptr, ids.len() as u32) == 0 {
                return Err(GetLastError());
            }
        }
        Ok(())
    }

    fn cpu_sets(&mut self, handle: Handle) -> Option<Vec<u32>> {
        let mut ids = [0u32; 64];
        let mut count = 0u32;
        let ok =
            unsafe { GetProcessDefaultCpuSets(handle as HANDLE, ids.as_mut_ptr(), 64, &mut count) };
        (ok != 0).then(|| ids[..count as usize].to_vec())
    }

    fn log(&mut self, msg: &str) {
        crate::log::log(msg);
    }
}
