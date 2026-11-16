//! Turning what the user pointed at (an `.exe`, a `.lnk` shortcut, a `.url`
//! link) into the program a rule should be created for.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};

use crate::win::{from_wide, wide};

pub const NOT_A_PROGRAM: &str =
    "This shortcut does not point at an executable (for example a Steam or Store link), \
     so Pinfold cannot tell which program it starts.\n\n\
     Start the program once and assign it from the tray menu under Assign.";

/// The executable path for a dropped or given path.
/// `.exe` is taken as is, `.lnk` is resolved to its target, anything else
/// is an error with a message for the user.
pub fn program_for(path: &str) -> Result<String, String> {
    program_for_with(path, resolve_lnk)
}

fn has_ext(path: &str, ext: &str) -> bool {
    path.to_lowercase().ends_with(ext)
}

fn program_for_with(
    path: &str,
    resolve: impl Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    if has_ext(path, ".exe") {
        return Ok(path.to_string());
    }
    if has_ext(path, ".url") {
        return Err(NOT_A_PROGRAM.to_string());
    }
    if has_ext(path, ".lnk") {
        let target = resolve(path)?;
        if target.is_empty() {
            // Store apps and advertised (installer) shortcuts have no path.
            return Err(NOT_A_PROGRAM.to_string());
        }
        if !has_ext(&target, ".exe") {
            return Err(format!(
                "The shortcut points at something that is not a program:\n{target}"
            ));
        }
        return Ok(target);
    }
    Err(format!("Not a program or shortcut:\n{path}"))
}

// --- IShellLinkW through raw COM vtables ------------------------------------

const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_C000_000000000046);
const IID_ISHELL_LINK_W: GUID = GUID::from_u128(0x000214F9_0000_0000_C000_000000000046);
const IID_IPERSIST_FILE: GUID = GUID::from_u128(0x0000010B_0000_0000_C000_000000000046);

// Vtable slots (after IUnknown's QueryInterface, AddRef, Release).
const UNKNOWN_QUERY_INTERFACE: usize = 0;
const UNKNOWN_RELEASE: usize = 2;
const SHELL_LINK_GET_PATH: usize = 3;
const PERSIST_FILE_LOAD: usize = 5;

type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type GetPathFn = unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> HRESULT;
type LoadFn = unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT;

unsafe fn slot(object: *mut c_void, index: usize) -> *const c_void {
    let vtable = *(object as *const *const *const c_void);
    *vtable.add(index)
}

unsafe fn release(object: *mut c_void) {
    let f: ReleaseFn = std::mem::transmute(slot(object, UNKNOWN_RELEASE));
    f(object);
}

/// Target path of a `.lnk` file; empty when the shortcut has no file target.
fn resolve_lnk(path: &str) -> Result<String, String> {
    let failed =
        |what: &str, hr: HRESULT| format!("Cannot read the shortcut ({what}, 0x{hr:08X}).");
    unsafe {
        let init = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let result = (|| {
            let mut link: *mut c_void = null_mut();
            let hr = CoCreateInstance(
                &CLSID_SHELL_LINK,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ISHELL_LINK_W,
                &mut link,
            );
            if hr < 0 || link.is_null() {
                return Err(failed("create", hr));
            }
            let mut file: *mut c_void = null_mut();
            let query: QueryInterfaceFn = std::mem::transmute(slot(link, UNKNOWN_QUERY_INTERFACE));
            let hr = query(link, &IID_IPERSIST_FILE, &mut file);
            if hr < 0 || file.is_null() {
                release(link);
                return Err(failed("query", hr));
            }
            let load: LoadFn = std::mem::transmute(slot(file, PERSIST_FILE_LOAD));
            let hr = load(file, wide(path).as_ptr(), 0);
            release(file);
            if hr < 0 {
                release(link);
                return Err(failed("load", hr));
            }
            let mut buf = [0u16; 1024];
            let get_path: GetPathFn = std::mem::transmute(slot(link, SHELL_LINK_GET_PATH));
            let hr = get_path(link, buf.as_mut_ptr(), buf.len() as i32, null_mut(), 0);
            release(link);
            if hr < 0 {
                return Err(failed("path", hr));
            }
            Ok(from_wide(&buf))
        })();
        if init >= 0 {
            CoUninitialize();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never(_: &str) -> Result<String, String> {
        panic!("resolver must not be called")
    }

    #[test]
    fn exe_is_taken_as_is() {
        assert_eq!(program_for_with("D:\\G\\Game.EXE", never).unwrap(), "D:\\G\\Game.EXE");
    }

    #[test]
    fn url_shortcut_gets_the_explanation() {
        assert_eq!(program_for_with("C:\\Desktop\\Game.url", never).unwrap_err(), NOT_A_PROGRAM);
    }

    #[test]
    fn lnk_is_resolved_to_its_target() {
        let r = program_for_with("a.LNK", |_| Ok("D:\\G\\game.exe".into()));
        assert_eq!(r.unwrap(), "D:\\G\\game.exe");
    }

    #[test]
    fn lnk_without_file_target_gets_the_explanation() {
        let r = program_for_with("store-app.lnk", |_| Ok(String::new()));
        assert_eq!(r.unwrap_err(), NOT_A_PROGRAM);
    }

    #[test]
    fn lnk_to_a_document_is_rejected() {
        let r = program_for_with("notes.lnk", |_| Ok("C:\\notes.txt".into()));
        assert!(r.unwrap_err().contains("not a program"));
    }

    #[test]
    fn unreadable_lnk_reports_the_error() {
        let r = program_for_with("x.lnk", |_| Err("Cannot read the shortcut".into()));
        assert!(r.unwrap_err().contains("Cannot read"));
    }

    #[test]
    fn other_files_are_rejected() {
        assert!(program_for_with("readme.txt", never).is_err());
    }
}
