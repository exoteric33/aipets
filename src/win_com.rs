//! The little COM aipets needs: an apartment per thread and the display name of a shell item.

use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::UI::Shell::{SHGetNameFromIDList, SHParseDisplayName, SIGDN_NORMALDISPLAY};

use crate::win;

/// COM for this thread (single-threaded apartment) while the guard lives.
pub struct ComInit(bool);

impl ComInit {
    pub fn new() -> ComInit {
        let hr = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        ComInit(hr >= 0)
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Display name of a shell item ("shell:AppsFolder\…" = a Start menu app), or None.
pub fn shell_display_name(parsing_name: &str) -> Option<String> {
    let _com = ComInit::new();
    unsafe {
        let mut pidl = std::ptr::null_mut();
        if SHParseDisplayName(win::wide(parsing_name).as_ptr(), std::ptr::null_mut(), &mut pidl, 0, std::ptr::null_mut()) < 0 || pidl.is_null() {
            return None;
        }
        let mut name: *mut u16 = std::ptr::null_mut();
        let ok = SHGetNameFromIDList(pidl, SIGDN_NORMALDISPLAY, &mut name) >= 0;
        CoTaskMemFree(pidl as *const _);
        if !ok || name.is_null() {
            return None;
        }
        let s = win::from_wide_ptr(name);
        CoTaskMemFree(name as *const _);
        Some(s)
    }
}
