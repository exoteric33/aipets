//! The two Windows dialogs aipets uses: pick a folder (working folder) and pick an exe (desktop app).

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, MAX_PATH};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;

use crate::win;
use crate::win_com::ComInit;

/// The folder dialog, starting at `initial`. None when cancelled or not a file system folder.
pub fn browse_folder(owner: HWND, title: &str, initial: &str) -> Option<String> {
    unsafe extern "system" fn callback(hwnd: HWND, msg: u32, _: LPARAM, data: LPARAM) -> i32 {
        if msg == BFFM_INITIALIZED && data != 0 {
            SendMessageW(hwnd, BFFM_SETSELECTIONW, 1, data);
        }
        0
    }
    let _com = ComInit::new();
    let title = win::wide(title);
    let start = win::wide(initial);
    let mut name = [0u16; MAX_PATH as usize];
    let info = BROWSEINFOW {
        hwndOwner: owner,
        pidlRoot: null_mut(),
        pszDisplayName: name.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        lpfn: Some(callback),
        lParam: if initial.is_empty() { 0 } else { start.as_ptr() as LPARAM },
        iImage: 0,
    };
    unsafe {
        let pidl = SHBrowseForFolderW(&info);
        if pidl.is_null() {
            return None;
        }
        let mut path = [0u16; 1024];
        let ok = SHGetPathFromIDListEx(pidl, path.as_mut_ptr(), path.len() as u32, 0) != 0;
        CoTaskMemFree(pidl as *const _);
        let path = win::from_wide(&path);
        if ok && !path.is_empty() { Some(path) } else { None }
    }
}

/// The open dialog for an .exe, starting in `dir` with `file` preselected. None when cancelled.
pub fn open_exe(owner: HWND, title: &str, dir: &str, file: &str) -> Option<String> {
    let filter: Vec<u16> = "Programs (*.exe)\0*.exe\0\0".encode_utf16().collect();
    let title = win::wide(title);
    let dir = win::wide(dir);
    let mut buffer = vec![0u16; 4096];
    for (i, c) in file.encode_utf16().take(buffer.len() - 1).enumerate() {
        buffer[i] = c;
    }
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buffer.as_mut_ptr();
    ofn.nMaxFile = buffer.len() as u32;
    ofn.lpstrInitialDir = if dir.len() > 1 { dir.as_ptr() } else { null() };
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let path = win::from_wide(&buffer);
    if path.is_empty() { None } else { Some(path) }
}
