//! Message boxes and task dialogs, without `unsafe` at the call site.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDCBF_OK_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_ENABLE_HYPERLINKS, TDN_BUTTON_CLICKED,
    TDN_HYPERLINK_CLICKED, TD_INFORMATION_ICON,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_YESNO,
};

use crate::win::wide;

/// A message box with an OK button.
pub fn tell(title: &str, text: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(text).as_ptr(),
            wide(title).as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        );
    }
}

/// A message box with Yes and No. Returns true for Yes.
pub fn ask(title: &str, text: &str) -> bool {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(text).as_ptr(),
            wide(title).as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

/// A message box with the title "Pinfold".
pub fn message_box(text: &str) {
    tell("Pinfold", text);
}

/// A task dialog with an information icon.
#[derive(Default)]
pub struct TaskDialog<'a> {
    pub title: &'a str,
    pub heading: &'a str,
    /// With `links`, the text can have `<a href="...">` links.
    pub text: &'a str,
    pub links: bool,
    /// Own buttons (id, label). The first is the default. If empty, the
    /// dialog has one OK button.
    pub buttons: Vec<(i32, String)>,
    /// Called for a click on a button. Return true to keep the dialog open.
    pub on_button: Option<&'a dyn Fn(i32) -> bool>,
    /// Called for a click on a link, with its href.
    pub on_link: Option<&'a dyn Fn(&str)>,
}

impl TaskDialog<'_> {
    /// Show the dialog and wait. Returns the id of the button that closed
    /// it, or None if Windows cannot show a task dialog.
    pub fn show(&self) -> Option<i32> {
        let title = wide(self.title);
        let heading = wide(self.heading);
        let text = wide(self.text);
        let labels: Vec<Vec<u16>> = self.buttons.iter().map(|(_, l)| wide(l)).collect();
        let buttons: Vec<TASKDIALOG_BUTTON> = self
            .buttons
            .iter()
            .zip(&labels)
            .map(|((id, _), l)| TASKDIALOG_BUTTON { nButtonID: *id, pszButtonText: l.as_ptr() })
            .collect();
        let mut pressed = 0i32;
        unsafe {
            let mut cfg: TASKDIALOGCONFIG = std::mem::zeroed();
            cfg.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
            cfg.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION;
            if self.links {
                cfg.dwFlags |= TDF_ENABLE_HYPERLINKS;
            }
            cfg.pszWindowTitle = title.as_ptr();
            cfg.Anonymous1.pszMainIcon = TD_INFORMATION_ICON;
            cfg.pszMainInstruction = heading.as_ptr();
            cfg.pszContent = text.as_ptr();
            if buttons.is_empty() {
                cfg.dwCommonButtons = TDCBF_OK_BUTTON;
            } else {
                cfg.cButtons = buttons.len() as u32;
                cfg.pButtons = buttons.as_ptr();
                cfg.nDefaultButton = buttons[0].nButtonID;
            }
            cfg.pfCallback = Some(callback);
            cfg.lpCallbackData = self as *const TaskDialog as isize;
            if TaskDialogIndirect(&cfg, &mut pressed, null_mut(), null_mut()) < 0 {
                return None;
            }
        }
        Some(pressed)
    }
}

unsafe extern "system" fn callback(
    _hwnd: HWND,
    msg: i32,
    wparam: WPARAM,
    lparam: LPARAM,
    data: isize,
) -> i32 {
    // `data` is the TaskDialog that `show` borrows while the dialog is open.
    let dialog = &*(data as *const TaskDialog);
    match msg {
        TDN_BUTTON_CLICKED => {
            if let Some(f) = dialog.on_button {
                if f(wparam as i32) {
                    return 1; // S_FALSE: keep the dialog open
                }
            }
        }
        TDN_HYPERLINK_CLICKED => {
            if let Some(f) = dialog.on_link {
                let link = lparam as *const u16;
                let mut len = 0;
                while *link.add(len) != 0 {
                    len += 1;
                }
                f(&String::from_utf16_lossy(std::slice::from_raw_parts(link, len)));
            }
        }
        _ => {}
    }
    0
}
