//! Win32 windows with Rust state. Each window keeps its state in a RefCell behind GWLP_USERDATA.
//! Windows calls window procedures re-entrantly (SetWindowPos, DestroyWindow, menus and dialogs send
//! messages while a handler runs), so a handler only borrows the state for plain work, never across a
//! modal call, and a nested message that finds the state borrowed falls back to DefWindowProc.
//! A panic stays inside the window: logged by the panic hook, then the window type decides what to do.

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};

use windows_sys::core::PCWSTR;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::HBRUSH;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::win::{self, Rect};

pub struct Slot<T> {
    pub state: RefCell<T>,
    depth: Cell<u32>,
    destroyed: Cell<bool>,
}

impl<T> Slot<T> {
    /// Runs f with the state, unless a handler further up the stack holds it (then None).
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut state = self.state.try_borrow_mut().ok()?;
        Some(f(&mut state))
    }
}

pub trait Window: Sized + 'static {
    /// Handles a message; None leaves it to DefWindowProc.
    fn message(slot: &Slot<Self>, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT>;

    /// The handler panicked (already logged). The state is no longer borrowed.
    fn panicked(_slot: &Slot<Self>, _hwnd: HWND) {}
}

thread_local! {
    static ATTACHED: Cell<bool> = const { Cell::new(false) };
}

pub fn instance() -> HINSTANCE {
    unsafe { GetModuleHandleW(null()) }
}

/// Registers a window class for T (a second registration of the same name is harmless).
pub fn register<T: Window>(class: &str, cursor: PCWSTR, background: HBRUSH) {
    let name = win::wide(class);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(proc::<T>),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance(),
        hIcon: null_mut(),
        hCursor: if cursor.is_null() { null_mut() } else { unsafe { LoadCursorW(null_mut(), cursor) } },
        hbrBackground: background,
        lpszMenuName: null(),
        lpszClassName: name.as_ptr(),
        hIconSm: null_mut(),
    };
    unsafe { RegisterClassExW(&wc) };
}

/// Creates a window whose messages go to T::message with this state. Null on failure.
pub fn create<T: Window>(class: &str, title: &str, style: u32, ex_style: u32, r: Rect, parent: HWND, state: T) -> HWND {
    let slot = Box::into_raw(Box::new(Slot { state: RefCell::new(state), depth: Cell::new(0), destroyed: Cell::new(false) }));
    let before = ATTACHED.with(|a| a.replace(false));
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            win::wide(class).as_ptr(),
            win::wide(title).as_ptr(),
            style,
            r.x,
            r.y,
            r.w,
            r.h,
            parent,
            null_mut(),
            instance(),
            slot as *const std::ffi::c_void,
        )
    };
    let attached = ATTACHED.with(|a| a.replace(before));
    if hwnd.is_null() && !attached {
        drop(unsafe { Box::from_raw(slot) }); // never reached the window; once attached, WM_NCDESTROY frees it
    }
    hwnd
}

/// The state of a window created by create::<T>, for calls from outside its own messages.
pub fn with<T: Window, R>(hwnd: HWND, f: impl FnOnce(&mut T) -> R) -> Option<R> {
    if !win::is_window(hwnd) {
        return None;
    }
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Slot<T>;
    if ptr.is_null() {
        return None;
    }
    unsafe { &*ptr }.with(f)
}

unsafe extern "system" fn proc<T: Window>(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp as *const CREATESTRUCTW);
        if !cs.lpCreateParams.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            ATTACHED.with(|a| a.set(true));
        }
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Slot<T>;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let slot = &*ptr;
    slot.depth.set(slot.depth.get() + 1);
    let result = match catch_unwind(AssertUnwindSafe(|| T::message(slot, hwnd, msg, wp, lp))) {
        Ok(Some(r)) => r,
        Ok(None) => DefWindowProcW(hwnd, msg, wp, lp),
        Err(_) => {
            let _ = catch_unwind(AssertUnwindSafe(|| T::panicked(slot, hwnd)));
            if slot.destroyed.get() { 0 } else { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    };
    if msg == WM_NCDESTROY {
        slot.destroyed.set(true);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
    slot.depth.set(slot.depth.get() - 1);
    if slot.destroyed.get() && slot.depth.get() == 0 {
        drop(Box::from_raw(ptr as *mut Slot<T>)); // the outermost handler has returned: nobody uses it any more
    }
    result
}

/// Popup menus (pet and tray right-click menus).
pub mod menu {
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::HBITMAP;

    pub fn new() -> HMENU {
        unsafe { CreatePopupMenu() }
    }

    /// "&" would underline the next letter: names and paths keep their ampersands.
    fn text(s: &str) -> Vec<u16> {
        win::wide(&s.replace('&', "&&"))
    }

    pub fn item(menu: HMENU, id: u32, label: &str, checked: bool, enabled: bool) {
        let mut flags = MF_STRING;
        if checked {
            flags |= MF_CHECKED;
        }
        if !enabled {
            flags |= MF_GRAYED;
        }
        unsafe { AppendMenuW(menu, flags, id as usize, text(label).as_ptr()) };
    }

    pub fn submenu(menu: HMENU, label: &str, sub: HMENU) {
        unsafe { AppendMenuW(menu, MF_STRING | MF_POPUP, sub as usize, text(label).as_ptr()) };
    }

    pub fn separator(menu: HMENU) {
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, null()) };
    }

    /// Bold, like the default action of a menu.
    pub fn bold(menu: HMENU, id: u32) {
        unsafe { SetMenuDefaultItem(menu, id, 0) };
    }

    /// Picture in front of the last added entry (a 32-bit premultiplied bitmap the caller keeps alive).
    pub fn image(menu: HMENU, bitmap: HBITMAP) {
        if bitmap.is_null() {
            return;
        }
        unsafe {
            let count = GetMenuItemCount(menu);
            let mut info: MENUITEMINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MENUITEMINFOW>() as u32;
            info.fMask = MIIM_BITMAP;
            info.hbmpItem = bitmap;
            SetMenuItemInfoW(menu, (count - 1) as u32, 1, &info);
        }
    }

    /// Shows the menu at the cursor and returns the chosen id (0: none). The menu needs a foreground window
    /// of ours to close on a click elsewhere; afterwards the window that had the focus gets it back, unless
    /// the choice opens a window of its own (`keeps_focus`).
    pub fn track(menu: HMENU, owner: HWND, keeps_focus: &[u32]) -> u32 {
        unsafe {
            let previous = GetForegroundWindow();
            SetForegroundWindow(owner);
            let p = win::cursor_pos();
            let cmd = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, p.x, p.y, owner, null()) as u32;
            PostMessageW(owner, WM_NULL, 0, 0);
            DestroyMenu(menu);
            if !previous.is_null() && previous != owner && !keeps_focus.contains(&cmd) {
                SetForegroundWindow(previous);
            }
            cmd
        }
    }
}
