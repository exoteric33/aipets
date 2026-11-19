//! Messages between aipets windows. Pets are top-level windows titled "aipets.pet.<id>", the tray owns a
//! hidden window "aipets.host". To a pet: a registered message (reload, launch) or WM_CLOSE. To the tray
//! from another process (a second start, the uninstaller): WM_COPYDATA with a text command
//! ("settings claude", "hide hermes", "quit").

use std::sync::OnceLock;

use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::win;

pub const HOST_TITLE: &str = "aipets.host";
pub const PET_TITLE_PREFIX: &str = "aipets.pet.";
pub const CMD_RELOAD: usize = 1;
pub const CMD_LAUNCH: usize = 2;

pub fn command_message() -> u32 {
    static M: OnceLock<u32> = OnceLock::new();
    *M.get_or_init(|| win::register_message("aipets.command"))
}

pub fn pet_title(id: &str) -> String {
    format!("{PET_TITLE_PREFIX}{id}")
}

pub fn post_to_pet(id: &str, command: usize) -> bool {
    win::post(win::find_window(&pet_title(id)), command_message(), command, 0)
}

/// Sends a text command to a running tray (in another process). False if there is none.
pub fn send_to_host(text: &str) -> bool {
    let hwnd = win::find_window(HOST_TITLE);
    if hwnd.is_null() {
        return false;
    }
    // this process just got the click or the start: let the tray bring its settings window to the front
    win::allow_any_foreground();
    let data: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let cds = COPYDATASTRUCT { dwData: 0, cbData: (data.len() * 2) as u32, lpData: data.as_ptr() as *mut _ };
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(hwnd, WM_COPYDATA, 0, &cds as *const _ as isize, SMTO_ABORTIFHUNG, 3000, &mut result) != 0
    }
}

pub unsafe fn read_copy_data(lparam: isize) -> String {
    let cds = &*(lparam as *const COPYDATASTRUCT);
    if cds.cbData < 2 || cds.lpData.is_null() {
        return String::new();
    }
    let units = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len()); // with or without the closing null
    String::from_utf16_lossy(&units[..end])
}
