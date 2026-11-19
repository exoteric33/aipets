//! The tray's settings window: one page per pet, every change applies immediately.
//! Plain Win32 controls laid out like the WinForms version (700 × 574 at 96 DPI): the pet list on the left,
//! the page of the selected pet, a bar at the bottom.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetCapture, GetFocus, SetFocus};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, autostart, store, Ini};
use crate::atlas;
use crate::dialogs;
use crate::json;
use crate::launcher::{self, DesktopApp};
use crate::pets::{self, PetInfo, PetSettings, HEIGHT_UNIT, MIN_HEIGHT};
use crate::setup;
use crate::tray::{self, HostMsg};
use crate::win::{self, Rect};
use crate::wnd::{self, Slot, Window};

const CLASS: &str = "aipets.settings";
const TITLE_TEXT: &str = "aipets – Settings";
const SHELL_VALUES: [&str; 3] = ["direct", "powershell", "cmd"];
const SHELL_NAMES: [&str; 3] = ["Windows Terminal", "Windows Terminal · PowerShell", "Windows Terminal · Command Prompt"];
const STANDARD: &str = "Standard (no arguments)";
const BYPASS: &str = "Bypass permissions";
const CUSTOM: &str = "Custom arguments";

// static styles and one trackbar message windows-sys keeps elsewhere
const SS_NOPREFIX: u32 = 0x80;
const SS_EDITCONTROL: u32 = 0x2000;
const SS_ENDELLIPSIS: u32 = 0x4000;
const TBM_GETPOS: u32 = WM_USER;

const fn cref(r: u32, g: u32, b: u32) -> u32 {
    r | g << 8 | b << 16
}
const PANE: u32 = cref(243, 243, 243);
const SELECTION: u32 = cref(230, 230, 230);
const ACCENT: u32 = cref(0, 103, 192);
const MUTED: u32 = cref(100, 100, 100);
const GOOD: u32 = cref(16, 124, 16);
const LABEL: u32 = cref(40, 40, 40);
const DARK: u32 = cref(26, 26, 26);
const CONTROL_LIGHT: u32 = cref(227, 227, 227);

// layout at 96 DPI: side panel, page, bottom bar
const WIDTH: i32 = 700;
const HEIGHT: i32 = 574;
const SIDE_W: i32 = 210;
const BAR_H: i32 = 56;
const BAR_Y: i32 = HEIGHT - BAR_H;
const PAGE_X: i32 = SIDE_W;

const WM_REFRESH: u32 = WM_APP + 10;
const WM_RESHOW: u32 = WM_APP + 11;
const SIZE_TIMER: usize = 1;

// controls (never 1 or 2: IDOK and IDCANCEL come from Enter and Esc)
const LIST: u16 = 100;
const TITLE: u16 = 101;
const STATE: u16 = 102;
const SHOW: u16 = 103;
const L_SIZE: u16 = 110;
const SIZE_BAR: u16 = 111;
const SIZE_VALUE: u16 = 112;
const L_PET_SIZE: u16 = 113;
const PET_SIZE_BAR: u16 = 114;
const PET_SIZE_VALUE: u16 = 115;
const LIKE_ALL: u16 = 116;
const L_OPENS: u16 = 120;
const WEBSITE: u16 = 121;
const APP: u16 = 122;
const PROGRAM: u16 = 123;
const L_PROGRAM: u16 = 130;
const PROGRAM_BOX: u16 = 131;
const L_LINK: u16 = 132;
const URL_BOX: u16 = 133;
const L_APP: u16 = 134;
const APP_NAME: u16 = 135;
const PICK_APP: u16 = 136;
const APP_WHERE: u16 = 137;
const RESET_APP: u16 = 138;
const L_CLI: u16 = 140;
const ARGS_PRESET: u16 = 141;
const L_BROWSER: u16 = 142;
const BROWSER: u16 = 143;
const RESET_URL: u16 = 144;
const L_ARGS: u16 = 150;
const ARGS_BOX: u16 = 151;
const L_SHELL: u16 = 160;
const SHELL_BOX: u16 = 161;
const RESET_PROGRAM: u16 = 162;
const L_FOLDER: u16 = 170;
const DIR_BOX: u16 = 171;
const BROWSE: u16 = 172;
const L_STATUS: u16 = 180;
const HOOKS: u16 = 181;
const SETUP: u16 = 182;
const OPEN: u16 = 190;
const HOME: u16 = 191;
const AUTOSTART: u16 = 200;
const HIDE_ALL: u16 = 201;
const LOG: u16 = 202;
const CLOSE: u16 = 203;

// "Click opens: Program" shows program, arguments, terminal and folder; "Desktop app" the app (and the folder
// if the program opens the app: codex app); "Website" only the link
const PROGRAM_ROWS: [u16; 9] = [L_PROGRAM, PROGRAM_BOX, L_CLI, ARGS_PRESET, L_ARGS, ARGS_BOX, L_SHELL, SHELL_BOX, RESET_PROGRAM];
const APP_ROWS: [u16; 5] = [L_APP, APP_NAME, PICK_APP, APP_WHERE, RESET_APP];
const LINK_ROWS: [u16; 5] = [L_LINK, URL_BOX, L_BROWSER, BROWSER, RESET_URL];
const FOLDER_ROWS: [u16; 3] = [L_FOLDER, DIR_BOX, BROWSE];

/// Fonts and brushes, made once at the system DPI.
struct Look {
    k: f64,
    font: HFONT,
    name_font: HFONT,
    title_font: HFONT,
    white: HBRUSH,
    pane: HBRUSH,
    selection: HBRUSH,
    accent: HBRUSH,
    line: HPEN,
}

static HWND_NOW: AtomicIsize = AtomicIsize::new(0);
static SETTING_UP: AtomicBool = AtomicBool::new(false);

thread_local! {
    static LOOK: RefCell<Option<Look>> = const { RefCell::new(None) };
    // what painting needs: never behind the window's state, which may be busy when a control repaints
    static PETS: RefCell<Vec<PetInfo>> = const { RefCell::new(Vec::new()) };
    static ICONS: RefCell<HashMap<String, HBITMAP>> = RefCell::new(HashMap::new());
    static AVATAR: RefCell<Option<PetInfo>> = const { RefCell::new(None) };
    static APP_NAME_COLOR: Cell<u32> = const { Cell::new(DARK) };
    static HOOKS_COLOR: Cell<u32> = const { Cell::new(MUTED) };
    static SNAPSHOT: Cell<bool> = const { Cell::new(false) };
}

fn look<R>(f: impl FnOnce(&Look) -> R) -> R {
    LOOK.with(|l| {
        let mut l = l.borrow_mut();
        let look = l.get_or_insert_with(|| {
            let dpi = win::system_dpi();
            unsafe {
                Look {
                    k: dpi as f64 / 96.0,
                    font: win::create_font("Segoe UI", 9.0, 400, dpi),
                    name_font: win::create_font("Segoe UI", 10.0, 600, dpi),
                    title_font: win::create_font("Segoe UI", 17.0, 600, dpi),
                    white: CreateSolidBrush(cref(255, 255, 255)),
                    pane: CreateSolidBrush(PANE),
                    selection: CreateSolidBrush(SELECTION),
                    accent: CreateSolidBrush(ACCENT),
                    line: CreatePen(PS_SOLID, 1, CONTROL_LIGHT),
                }
            }
        });
        f(look)
    })
}

fn k() -> f64 {
    look(|l| l.k)
}

/// A length at 96 DPI in this window's pixels.
fn s(v: i32) -> i32 {
    (v as f64 * k()).round() as i32
}

fn sr(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect::new(s(x), s(y), s(w), s(h))
}

/// The settings window, or null.
pub fn dialog_hwnd() -> HWND {
    HWND_NOW.load(Ordering::Relaxed) as HWND
}

/// The tray changed something (every second, and after its own changes): show it.
pub fn refresh() {
    win::post(dialog_hwnd(), WM_REFRESH, 0, 0);
}

pub fn close() {
    let hwnd = dialog_hwnd();
    if !hwnd.is_null() {
        unsafe { SendMessageW(hwnd, WM_CLOSE, 0, 0) };
    }
}

/// Opens the window (or brings it to the front) on the page of this pet.
pub fn open(pet_id: Option<&str>) {
    let mut hwnd = dialog_hwnd();
    if hwnd.is_null() {
        hwnd = create(false);
        if hwnd.is_null() {
            return;
        }
    }
    wnd::with::<Settings, _>(hwnd, |s| s.select_pet(pet_id));
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

fn create(snapshot: bool) -> HWND {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES | ICC_LINK_CLASS,
        };
        InitCommonControlsEx(&icc);
    }
    wnd::register::<Settings>(CLASS, IDC_ARROW, null_mut());
    let pets = if snapshot { PetInfo::discover() } else { tray::pets() };
    PETS.with(|p| *p.borrow_mut() = pets.clone());
    SNAPSHOT.with(|c| c.set(snapshot));
    // like the WinForms version: scaled for the system DPI (Windows stretches it on other screens)
    let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) };
    let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    let ex = WS_EX_CONTROLPARENT;
    // a snapshot gets a hidden owner: no taskbar button, but the normal caption
    let owner = if snapshot {
        unsafe {
            CreateWindowExW(0, win::wide("STATIC").as_ptr(), null(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), wnd::instance(), null())
        }
    } else {
        null_mut()
    };
    let mut r = RECT { left: 0, top: 0, right: s(WIDTH), bottom: s(HEIGHT) };
    unsafe { AdjustWindowRectEx(&mut r, style, 0, ex) };
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let at = if snapshot {
        Rect::new(-32000, -32000, w, h)
    } else {
        let wa = win::work_area_at(win::cursor_pos());
        Rect::new(wa.x + (wa.w - w) / 2, wa.y + (wa.h - h) / 2, w, h)
    };
    let state = Settings {
        hwnd: null_mut(),
        snapshot,
        snapshot_mode: None,
        pets,
        pending_shared: 0,
        pending_own: 0,
        pending_own_pet: None,
        own_height: false,
        current: None,
        shown_gen: 0,
        closing: false,
    };
    let hwnd = wnd::create(CLASS, TITLE_TEXT, style, ex, at, owner, state);
    unsafe { SetThreadDpiAwarenessContext(previous) };
    if hwnd.is_null() {
        app::log("the settings window could not be created");
        if !owner.is_null() {
            unsafe { DestroyWindow(owner) };
        }
        return hwnd;
    }
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, win::load_app_icon(GetSystemMetrics(SM_CXICON)) as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, win::load_app_icon(GetSystemMetrics(SM_CXSMICON)) as isize);
    }
    if !snapshot {
        HWND_NOW.store(hwnd as isize, Ordering::Relaxed);
    }
    hwnd
}

// ------------------------------------------------------------------ small control helpers

fn wide_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn set_text(h: HWND, text: &str) {
    if wide_text(h) != text {
        unsafe { SetWindowTextW(h, win::wide(text).as_ptr()) };
    }
}

fn show(h: HWND, visible: bool) {
    unsafe { ShowWindow(h, if visible { SW_SHOW } else { SW_HIDE }) };
}

fn enable(h: HWND, on: bool) {
    unsafe { EnableWindow(h, on as i32) };
}

fn check(h: HWND, on: bool) {
    unsafe { SendMessageW(h, BM_SETCHECK, if on { BST_CHECKED as usize } else { BST_UNCHECKED as usize }, 0) };
}

fn checked(h: HWND) -> bool {
    unsafe { SendMessageW(h, BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
}

/// Text size in pixels with this font.
fn measure(font: HFONT, text: &str) -> (i32, i32) {
    unsafe {
        let dc = GetDC(null_mut());
        let old = SelectObject(dc, font as HGDIOBJ);
        let w = win::wide(text);
        let mut size = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, w.as_ptr(), (w.len() - 1) as i32, &mut size);
        let mut tm: TEXTMETRICW = std::mem::zeroed();
        GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        (size.cx, size.cy.max(tm.tmHeight))
    }
}

fn link_markup(text: &str) -> String {
    format!("<a>{}</a>", text.replace('&', "&amp;").replace('<', "&lt;"))
}

fn trackbar_pos(h: HWND) -> i32 {
    unsafe { SendMessageW(h, TBM_GETPOS, 0, 0) as i32 }
}

fn set_slider(h: HWND, value: i32) {
    unsafe {
        let min = SendMessageW(h, TBM_GETRANGEMIN, 0, 0) as i32;
        let max = SendMessageW(h, TBM_GETRANGEMAX, 0, 0) as i32;
        SendMessageW(h, TBM_SETPOS, 1, value.clamp(min, max) as isize);
    }
}

fn tallest_screen() -> i32 {
    win::all_work_areas().iter().map(|r| r.h).max().unwrap_or(0)
}

fn pet_bitmap(info: &PetInfo, size: i32) -> HBITMAP {
    ICONS.with(|icons| {
        *icons.borrow_mut().entry(format!("{}/{size}", info.id)).or_insert_with(|| {
            let icon = win::load_icon_file(&info.icon_path(), size);
            let bmp = win::icon_bitmap(icon, size);
            if !icon.is_null() {
                unsafe { DestroyIcon(icon) };
            }
            bmp
        })
    })
}

/// A premultiplied bitmap stretched without smoothing (pixel art stays crisp).
fn draw_bitmap(dc: HDC, bmp: HBITMAP, size: i32, dest: Rect) {
    if bmp.is_null() {
        return;
    }
    unsafe {
        let mem = CreateCompatibleDC(dc);
        let old = SelectObject(mem, bmp as HGDIOBJ);
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        SetStretchBltMode(dc, COLORONCOLOR);
        AlphaBlend(dc, dest.x, dest.y, dest.w, dest.h, mem, 0, 0, size, size, blend);
        SelectObject(mem, old);
        DeleteDC(mem);
    }
}

fn text_out(dc: HDC, font: HFONT, color: u32, x: i32, y: i32, text: &str) {
    unsafe {
        let old = SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        let w: Vec<u16> = text.encode_utf16().collect();
        TextOutW(dc, x, y, w.as_ptr(), w.len() as i32);
        SelectObject(dc, old);
    }
}

fn fill(dc: HDC, r: Rect, brush: HBRUSH) {
    let rc = RECT { left: r.x, top: r.y, right: r.right(), bottom: r.bottom() };
    unsafe { FillRect(dc, &rc, brush) };
}

// ------------------------------------------------------------------ the window

pub struct Settings {
    hwnd: HWND,
    snapshot: bool,
    snapshot_mode: Option<String>, // snapshot only: show the page in this mode instead of the saved one
    pets: Vec<PetInfo>,
    // slider moves are written a moment later, so a drag does not rewrite settings.ini for every pixel
    pending_shared: i32, // 0 = nothing to write
    pending_own: i32,
    pending_own_pet: Option<String>,
    own_height: bool, // the shown pet has her own height
    current: Option<usize>,
    shown_gen: u64,
    closing: bool,
}

impl Settings {
    fn ctl(&self, id: u16) -> HWND {
        unsafe { GetDlgItem(self.hwnd, id as i32) }
    }

    fn add(&self, class: &str, text: &str, style: u32, r: Rect, id: u16) -> HWND {
        self.add_ex(0, class, text, style, r, id)
    }

    fn add_ex(&self, ex: u32, class: &str, text: &str, style: u32, r: Rect, id: u16) -> HWND {
        unsafe {
            let h = CreateWindowExW(
                ex,
                win::wide(class).as_ptr(),
                win::wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                r.x,
                r.y,
                r.w,
                r.h,
                self.hwnd,
                id as usize as HMENU,
                wnd::instance(),
                null(),
            );
            SendMessageW(h, WM_SETFONT, look(|l| l.font) as usize, 0);
            h
        }
    }

    /// A label that is as wide as its text (AutoSize), top-left at (x, y) on the 96-DPI grid.
    fn label(&self, text: &str, x: i32, y: i32, id: u16) -> HWND {
        let font = look(|l| l.font);
        let (w, h) = measure(font, text);
        self.add("STATIC", text, SS_NOPREFIX, Rect::new(s(x), s(y), w + 1, h), id)
    }

    /// Sets a label's text and makes it as wide as the text.
    fn set_label(&self, id: u16, text: &str, font: HFONT) {
        let h = self.ctl(id);
        let (w, ht) = measure(font, text);
        unsafe { SetWindowPos(h, null_mut(), 0, 0, w + 1, ht, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
        set_text(h, text);
    }

    fn link(&self, text: &str, x: i32, y: i32, id: u16) -> HWND {
        let (w, h) = measure(look(|l| l.font), text);
        self.add("SysLink", &link_markup(text), WS_TABSTOP | LWS_TRANSPARENT, Rect::new(s(x), s(y), w + s(4), h + 2), id)
    }

    fn checkbox(&self, text: &str, x: i32, y: i32, id: u16, style: i32) -> HWND {
        let (w, h) = measure(look(|l| l.font), text);
        self.add("BUTTON", text, WS_TABSTOP | style as u32, Rect::new(s(x), s(y), w + s(22), h.max(s(17))), id)
    }

    fn edit(&self, x: i32, y: i32, w: i32, id: u16) -> HWND {
        // the client edge from the start: the theme then draws its own thin border
        self.add_ex(WS_EX_CLIENTEDGE, "EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, sr(x, y, w, 23), id)
    }

    fn combo(&self, x: i32, y: i32, w: i32, id: u16) -> HWND {
        self.add("COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32, sr(x, y, w, 200), id)
    }

    fn button(&self, text: &str, r: Rect, id: u16) -> HWND {
        self.add("BUTTON", text, WS_TABSTOP | BS_PUSHBUTTON as u32, r, id)
    }

    fn slider(&self, x: i32, y: i32, id: u16) -> HWND {
        let h = self.add("msctls_trackbar32", "", WS_TABSTOP | TBS_AUTOTICKS | TBS_BOTTOM, sr(x, y, 220, 34), id);
        unsafe {
            SendMessageW(h, TBM_SETRANGEMIN, 0, MIN_HEIGHT as isize);
            SendMessageW(h, TBM_SETRANGEMAX, 1, (MIN_HEIGHT + 1).max(tallest_screen()) as isize);
            SendMessageW(h, TBM_SETLINESIZE, 0, 10);
            SendMessageW(h, TBM_SETPAGESIZE, 0, (HEIGHT_UNIT / 2) as isize);
            SendMessageW(h, TBM_SETTICFREQ, HEIGHT_UNIT as usize, 0); // ticks at 162, 324, 486 … px
        }
        h
    }

    fn build(&mut self, hwnd: HWND) {
        self.hwnd = hwnd;
        let p = PAGE_X;
        // the pet list in the side panel (padding 8, 10)
        let list = self.add(
            "LISTBOX",
            "",
            WS_TABSTOP | WS_VSCROLL | (LBS_OWNERDRAWFIXED | LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | LBS_HASSTRINGS) as u32,
            sr(8, 10, SIDE_W - 16, BAR_Y - 20),
            LIST,
        );
        unsafe {
            SendMessageW(list, LB_SETITEMHEIGHT, 0, s(58) as isize);
            for pet in &self.pets {
                SendMessageW(list, LB_ADDSTRING, 0, win::wide(&pet.name).as_ptr() as isize);
            }
        }

        // the page: picture, name, state, "Show"
        let title = self.add("STATIC", "", SS_NOPREFIX, sr(p + 130, 26, 300, 40), TITLE);
        unsafe { SendMessageW(title, WM_SETFONT, look(|l| l.title_font) as usize, 0) };
        self.label("", p + 133, 64, STATE);
        self.checkbox("Show", p + 135, 88, SHOW, BS_AUTOCHECKBOX);

        // two height sliders, both live: the pets follow while a slider moves. All pets: every pet snaps to it,
        // own heights go. This pet: her own height until the next move of the first slider; "like all" gives it back.
        let mut y = 128;
        self.label("Size of all pets", p + 22, y, L_SIZE);
        self.slider(p + 134, y - 6, SIZE_BAR);
        self.label("", p + 360, y, SIZE_VALUE);
        y += 36;
        self.label("Size of", p + 22, y, L_PET_SIZE);
        self.slider(p + 134, y - 6, PET_SIZE_BAR);
        self.label("", p + 360, y, PET_SIZE_VALUE);
        self.link("like all", p + 424, y, LIKE_ALL);

        y += 36;
        self.label("Click opens", p + 22, y, L_OPENS);
        self.checkbox("Website", p + 140, y - 1, WEBSITE, BS_RADIOBUTTON | WS_GROUP as i32);
        self.checkbox("Desktop app", p + 248, y - 1, APP, BS_RADIOBUTTON);
        self.checkbox("Program", p + 380, y - 1, PROGRAM, BS_RADIOBUTTON);

        y += 34;
        self.label("Program", p + 22, y, L_PROGRAM);
        self.edit(p + 140, y - 3, 330, PROGRAM_BOX);
        self.label("Link", p + 22, y, L_LINK);
        self.edit(p + 140, y - 3, 330, URL_BOX);
        self.label("App", p + 22, y, L_APP);
        self.add("STATIC", "", SS_NOPREFIX | SS_ENDELLIPSIS, sr(p + 140, y, 292, 18), APP_NAME);
        self.button("…", sr(p + 436, y - 4, 34, 25), PICK_APP);
        self.add("STATIC", "", SS_NOPREFIX | SS_EDITCONTROL | SS_ENDELLIPSIS, sr(p + 140, y + 20, 292, 34), APP_WHERE);
        self.link("Reset app", p + 139, y + 59, RESET_APP);
        y += 34;
        self.label("CLI arguments", p + 22, y, L_CLI);
        self.combo(p + 140, y - 3, 330, ARGS_PRESET);
        self.label("Open in", p + 22, y, L_BROWSER);
        self.label("Default browser", p + 140, y, BROWSER);
        self.link("Reset link", p + 139, y + 25, RESET_URL);
        y += 34;
        self.label("Arguments", p + 22, y, L_ARGS);
        self.edit(p + 140, y - 3, 330, ARGS_BOX);
        y += 34;
        self.label("Open in", p + 22, y, L_SHELL);
        let shell = self.combo(p + 140, y - 3, 330, SHELL_BOX);
        for name in SHELL_NAMES {
            unsafe { SendMessageW(shell, CB_ADDSTRING, 0, win::wide(name).as_ptr() as isize) };
        }
        self.link("Reset program, arguments and \"Open in\"", p + 139, y + 25, RESET_PROGRAM);

        y += 56;
        self.label("Working folder", p + 22, y, L_FOLDER);
        self.edit(p + 140, y - 3, 292, DIR_BOX);
        self.button("…", sr(p + 436, y - 4, 34, 25), BROWSE);

        y += 38;
        self.label("Status display", p + 22, y, L_STATUS);
        self.add("STATIC", "", SS_NOPREFIX | SS_ENDELLIPSIS, sr(p + 140, y, 330, 18), HOOKS);
        self.link("Set up hooks", p + 139, y + 19, SETUP);

        y += 44;
        self.button("", sr(p + 140, y, 162, 30), OPEN);
        self.button("Back to the corner", sr(p + 308, y, 162, 30), HOME);

        // the bar
        let autostart_box = self.checkbox("Start with Windows", 20, BAR_Y + 18, AUTOSTART, BS_AUTOCHECKBOX);
        let hide_all = self.checkbox("Hide all pets", 190, BAR_Y + 18, HIDE_ALL, BS_AUTOCHECKBOX);
        self.link("Open log", 370, BAR_Y + 19, LOG);
        self.button("Close", sr(576, BAR_Y + 13, 104, 30), CLOSE);

        check(autostart_box, autostart::enabled());
        check(hide_all, pets::hides_all(&self.ini().0));
    }

    fn ini(&self) -> (Ini, u64) {
        if self.snapshot {
            return (store::load(), 0);
        }
        tray::ini().unwrap_or_else(|| (store::load(), 0))
    }

    fn saved(&self, info: &PetInfo) -> PetSettings {
        PetSettings::from(info, &self.ini().0)
    }

    fn state_text(&self, info: &PetInfo) -> String {
        if self.snapshot { "–".into() } else { tray::state_text(&info.id) }
    }

    fn select_pet(&mut self, id: Option<&str>) {
        let list = self.ctl(LIST);
        let found = id.and_then(|id| self.pets.iter().position(|p| win::eq_ignore_case(&p.id, id)));
        let selected = unsafe { SendMessageW(list, LB_GETCURSEL, 0, 0) };
        let index = found.unwrap_or(if selected >= 0 { selected as usize } else { 0 });
        if index < self.pets.len() {
            unsafe {
                SendMessageW(list, LB_SETCURSEL, index, 0);
                InvalidateRect(list, null(), 1);
            }
            self.list_changed();
        }
    }

    fn list_changed(&mut self) {
        let list = self.ctl(LIST);
        unsafe { InvalidateRect(list, null(), 1) };
        self.write_heights(); // a slider just moved on the page being left
        let i = unsafe { SendMessageW(list, LB_GETCURSEL, 0, 0) };
        if i >= 0 && (i as usize) < self.pets.len() {
            self.show_pet(i as usize);
        }
    }

    fn show_current(&mut self) {
        if let Some(i) = self.current {
            self.show_pet(i);
        }
    }

    fn show_pet(&mut self, i: usize) {
        self.current = Some(i);
        let (ini, gen) = self.ini();
        self.shown_gen = gen;
        let info = self.pets[i].clone();
        let mut st = PetSettings::from(&info, &ini);
        if let Some(mode) = &self.snapshot_mode {
            st.use_mode(mode);
        }
        let (font, title_font) = look(|l| (l.font, l.title_font));
        self.set_label(TITLE, &info.name, title_font);
        AVATAR.with(|a| *a.borrow_mut() = Some(info.clone()));
        let avatar = sr(PAGE_X + 22, 16, 96, 96);
        let rc = RECT { left: avatar.x, top: avatar.y, right: avatar.right(), bottom: avatar.bottom() };
        unsafe { InvalidateRect(self.hwnd, &rc, 0) };
        self.set_label(STATE, &self.state_text(&info), font);
        check(self.ctl(SHOW), st.enabled);
        enable(self.ctl(SHOW), !st.all_hidden); // her own choice waits until all pets are shown again
        check(self.ctl(HIDE_ALL), st.all_hidden);
        self.set_label(L_PET_SIZE, &format!("Size of {}", info.name), font);
        // not while a slider is dragged or its value still waits to be written: the thumb would jump back
        let capture = unsafe { GetCapture() };
        if self.pending_shared == 0 && self.pending_own == 0 && capture != self.ctl(SIZE_BAR) && capture != self.ctl(PET_SIZE_BAR) {
            self.own_height = st.own_height > 0;
            set_slider(self.ctl(SIZE_BAR), st.shared_height());
            set_slider(self.ctl(PET_SIZE_BAR), st.pet_height());
        }
        self.show_sizes();
        let focus = unsafe { GetFocus() };
        for (id, value) in [(PROGRAM_BOX, &st.program), (ARGS_BOX, &st.args), (DIR_BOX, &st.work_dir), (URL_BOX, &st.url)] {
            if focus != self.ctl(id) {
                set_text(self.ctl(id), value);
            }
        }
        let shell = SHELL_VALUES.iter().position(|v| *v == st.shell).unwrap_or(0);
        unsafe { SendMessageW(self.ctl(SHELL_BOX), CB_SETCURSEL, shell, 0) };
        let args = wide_text(self.ctl(ARGS_BOX));
        self.show_argument_presets(&info, &st.program, &args);
        check(self.ctl(PROGRAM), st.opens_program());
        check(self.ctl(APP), st.opens_app());
        check(self.ctl(WEBSITE), st.opens_website());
        let via_program = st.opens_app() && launcher::app_via_program(&info, &st);
        for id in PROGRAM_ROWS {
            show(self.ctl(id), st.opens_program());
        }
        for id in APP_ROWS {
            show(self.ctl(id), st.opens_app());
        }
        for id in LINK_ROWS {
            show(self.ctl(id), st.opens_website());
        }
        for id in FOLDER_ROWS {
            show(self.ctl(id), st.opens_program() || via_program);
        }
        if st.opens_app() {
            self.show_app(&info, &st, via_program);
        }
        set_text(self.ctl(OPEN), &info.open_text);
        let (text, ok) = hook_text(&info);
        HOOKS_COLOR.with(|c| c.set(if ok { GOOD } else { MUTED }));
        set_text(self.ctl(HOOKS), &text);
        unsafe { InvalidateRect(self.ctl(HOOKS), null(), 1) };
        show(self.ctl(SETUP), !ok && !info.status.is_empty());
    }

    /// Which app a click opens and how, or what happens without one.
    fn show_app(&self, pet: &PetInfo, st: &PetSettings, via_program: bool) {
        let app = DesktopApp::find(&st.desktop_app);
        APP_NAME_COLOR.with(|c| c.set(if app.is_some() { DARK } else { MUTED }));
        let name = match &app {
            Some(a) => a.to_string(),
            None if st.desktop_app.is_empty() => "Choose an app".into(),
            None => "not found".into(),
        };
        set_text(self.ctl(APP_NAME), &name);
        unsafe { InvalidateRect(self.ctl(APP_NAME), null(), 1) };
        let place = if via_program {
            format!("A click runs \"{}\" in the working folder, without a terminal window.", launcher::program_command(st, &pet.app_command))
        } else if let Some(a) = &app {
            match (&a.app_id, &a.exe) {
                (Some(_), _) => format!("App package {}", a.family().unwrap_or_default()),
                (None, Some(exe)) => app::short_path(exe),
                _ => String::new(),
            }
        } else if !pet.app_fallback.is_empty() {
            format!(
                "Instead, a click runs \"{}\" in the terminal, which sets up and opens the app.",
                launcher::program_command(st, &pet.app_fallback)
            )
        } else {
            "Install the app or pick its exe with \"…\".".into()
        };
        set_text(self.ctl(APP_WHERE), &place);
    }

    /// The heights behind the sliders; "like all" only does something for a pet with her own height.
    fn show_sizes(&self) {
        let font = look(|l| l.font);
        self.set_label(SIZE_VALUE, &pets::height_text(trackbar_pos(self.ctl(SIZE_BAR))), font);
        self.set_label(PET_SIZE_VALUE, &pets::height_text(trackbar_pos(self.ctl(PET_SIZE_BAR))), font);
        // a disabled SysLink still draws its link blue: without her own height it is plain grey text
        let like_all = self.ctl(LIKE_ALL);
        set_text(like_all, &if self.own_height { link_markup("like all") } else { "like all".to_string() });
        enable(like_all, self.own_height);
        unsafe { InvalidateRect(like_all, null(), 1) };
    }

    fn show_argument_presets(&self, pet: &PetInfo, program: &str, args: &str) {
        let bypass = pet.bypass_arguments(program);
        let combo = self.ctl(ARGS_PRESET);
        let mut items = vec![STANDARD];
        if !bypass.is_empty() {
            items.push(BYPASS);
        }
        items.push(CUSTOM);
        let chosen = if args.is_empty() {
            STANDARD
        } else if !bypass.is_empty() && args == bypass {
            BYPASS
        } else {
            CUSTOM
        };
        unsafe {
            SendMessageW(combo, CB_RESETCONTENT, 0, 0);
            for item in &items {
                SendMessageW(combo, CB_ADDSTRING, 0, win::wide(item).as_ptr() as isize);
            }
            SendMessageW(combo, CB_SETCURSEL, items.iter().position(|i| *i == chosen).unwrap_or(0), 0);
        }
    }

    /// Called by the tray every second: process states, and settings changed elsewhere (e.g. a pet's own menu).
    fn refresh_states(&mut self) {
        unsafe { InvalidateRect(self.ctl(LIST), null(), 0) };
        let Some(i) = self.current else { return };
        if self.snapshot {
            return;
        }
        if tray::ini_gen() != self.shown_gen {
            self.show_pet(i);
        } else {
            let text = self.state_text(&self.pets[i]);
            self.set_label(STATE, &text, look(|l| l.font));
        }
    }

    /// Writes the heights the sliders were moved to: a moment after the last move, or before leaving the page.
    fn write_heights(&mut self) {
        unsafe { KillTimer(self.hwnd, SIZE_TIMER) };
        if !self.snapshot && self.pending_shared > 0 {
            tray::set_height(self.pending_shared);
        }
        if !self.snapshot && self.pending_own > 0 {
            if let Some(id) = &self.pending_own_pet {
                tray::change_setting(id, &[("height", Some(&app::number(self.pending_own)))]);
            }
        }
        self.pending_shared = 0;
        self.pending_own = 0;
        self.pending_own_pet = None;
    }

    fn slider_moved(&mut self, bar: HWND) {
        if bar == self.ctl(SIZE_BAR) {
            if !self.snapshot {
                self.own_height = false;
                self.pending_own = 0; // an own height that is still waiting would be undone anyway
                self.pending_own_pet = None;
                self.pending_shared = trackbar_pos(bar);
                unsafe { SetTimer(self.hwnd, SIZE_TIMER, 80, None) };
            }
            if !self.own_height {
                set_slider(self.ctl(PET_SIZE_BAR), trackbar_pos(bar)); // she follows all pets
            }
            self.show_sizes();
        } else if bar == self.ctl(PET_SIZE_BAR) {
            if let (Some(i), false) = (self.current, self.snapshot) {
                self.own_height = true;
                self.pending_own = trackbar_pos(bar);
                self.pending_own_pet = Some(self.pets[i].id.clone());
                unsafe { SetTimer(self.hwnd, SIZE_TIMER, 80, None) };
            }
            self.show_sizes();
        }
    }

    fn current_pet(&self) -> Option<PetInfo> {
        if self.snapshot {
            return None; // nothing is changed from a snapshot
        }
        self.current.map(|i| self.pets[i].clone())
    }

    fn change(&mut self, values: &[(&str, Option<&str>)]) {
        if let Some(pet) = self.current_pet() {
            tray::change_setting(&pet.id, values);
            self.show_current();
        }
    }

    fn like_all(&mut self) {
        if self.current_pet().is_none() {
            return;
        }
        self.write_heights();
        self.change(&[("height", None)]);
    }

    fn mode_clicked(&mut self, id: u16) {
        let Some(pet) = self.current_pet() else { return };
        if checked(self.ctl(id)) {
            return;
        }
        let mode = match id {
            WEBSITE => "website",
            APP => "app",
            _ => "program",
        };
        tray::change_setting(&pet.id, &[("mode", if mode == pet.mode { None } else { Some(mode) })]);
        self.show_current();
    }

    fn preset_changed(&mut self) {
        let Some(pet) = self.current_pet() else { return };
        let combo = self.ctl(ARGS_PRESET);
        let sel = unsafe { SendMessageW(combo, CB_GETCURSEL, 0, 0) };
        if sel < 0 {
            return;
        }
        let mut buf = [0u16; 64];
        unsafe { SendMessageW(combo, CB_GETLBTEXT, sel as usize, buf.as_mut_ptr() as isize) };
        let choice = win::from_wide(&buf);
        let args_box = self.ctl(ARGS_BOX);
        if choice == CUSTOM {
            unsafe {
                SetFocus(args_box);
                SendMessageW(args_box, EM_SETSEL, 0, -1);
            }
            return;
        }
        let saved = self.saved(&pet);
        let value = if choice == BYPASS { pet.bypass_arguments(&saved.program) } else { String::new() };
        set_text(args_box, &value);
        tray::change_setting(&pet.id, &[("args", if value == pet.args { None } else { Some(&value) })]);
    }

    fn shell_changed(&mut self) {
        let Some(pet) = self.current_pet() else { return };
        let sel = unsafe { SendMessageW(self.ctl(SHELL_BOX), CB_GETCURSEL, 0, 0) };
        if sel < 0 || sel as usize >= SHELL_VALUES.len() {
            return;
        }
        let value = SHELL_VALUES[sel as usize];
        tray::change_setting(&pet.id, &[("shell", if value == pet.shell { None } else { Some(value) })]);
    }

    fn commit(&mut self, id: u16) {
        if self.closing {
            return;
        }
        let Some(info) = self.current_pet() else { return };
        let field = self.ctl(id);
        let mut value = wide_text(field).trim().to_string();
        let key = match id {
            PROGRAM_BOX => "program",
            ARGS_BOX => "args",
            URL_BOX => "url",
            _ => "workdir",
        };
        let (ini, _) = self.ini();
        let saved = PetSettings::from(&info, &ini);
        let shown = match key {
            "url" => saved.opens_website(),
            "workdir" => saved.opens_program() || (saved.opens_app() && launcher::app_via_program(&info, &saved)),
            _ => saved.opens_program(),
        };
        if !shown {
            return; // the fields of the other modes are hidden
        }
        if key == "workdir" && (value.is_empty() || !win::dir_exists(&value)) {
            set_text(field, &saved.work_dir); // not a folder: back to the saved one
            return;
        }
        if key == "url" {
            match launcher::normalize_url(&value) {
                Some(url) => {
                    value = url;
                    set_text(field, &value);
                }
                None => {
                    set_text(field, &saved.url); // not a link: back to the saved one
                    return;
                }
            }
        }
        let fallback = match key {
            "program" => Some(&info.program),
            "args" => Some(&info.args),
            "url" => Some(&info.url),
            _ => None,
        };
        let stored: Option<&str> = if fallback == Some(&value) { None } else { Some(&value) };
        let args_box = self.ctl(ARGS_BOX);
        // a preset belongs to its CLI: keep custom arguments, but do not carry a recognized bypass flag over to
        // an unrelated program when its name is changed
        if key == "program"
            && !saved.args.is_empty()
            && saved.args == info.bypass_arguments(&saved.program)
            && info.bypass_arguments(&value).is_empty()
            && wide_text(args_box).trim() == saved.args
        {
            tray::change_setting(&info.id, &[(key, stored), ("args", if info.args.is_empty() { None } else { Some("") })]);
            set_text(args_box, "");
        } else if stored != ini.get(&info.id, key) {
            tray::change_setting(&info.id, &[(key, stored)]);
        }
        if key == "program" || key == "args" {
            let program = self.saved(&info).program;
            self.show_argument_presets(&info, &program, wide_text(args_box).trim());
        }
    }

    fn commit_all(&mut self) {
        for id in [PROGRAM_BOX, ARGS_BOX, DIR_BOX, URL_BOX] {
            self.commit(id);
        }
    }

    fn command(&mut self, id: u16, code: u32) {
        match id {
            1 => {
                // Enter: take what was typed in the field that has the focus
                let focus = unsafe { GetFocus() };
                if let Some(field) = [PROGRAM_BOX, ARGS_BOX, DIR_BOX, URL_BOX].into_iter().find(|f| self.ctl(*f) == focus) {
                    self.commit(field);
                }
            }
            LIST if code == LBN_SELCHANGE => self.list_changed(),
            SHOW if code == BN_CLICKED as u32 => {
                if let Some(pet) = self.current_pet() {
                    tray::set_enabled(&pet.id, checked(self.ctl(SHOW)));
                }
            }
            AUTOSTART if code == BN_CLICKED as u32 && !self.snapshot => {
                if let Err(e) = autostart::choose(checked(self.ctl(AUTOSTART))) {
                    app::log(&format!("autostart: {e}"));
                }
            }
            HIDE_ALL if code == BN_CLICKED as u32 && !self.snapshot => tray::set_all_hidden(checked(self.ctl(HIDE_ALL))),
            WEBSITE | APP | PROGRAM if code == BN_CLICKED as u32 => self.mode_clicked(id),
            ARGS_PRESET if code == CBN_SELCHANGE => self.preset_changed(),
            SHELL_BOX if code == CBN_SELCHANGE => self.shell_changed(),
            PROGRAM_BOX | ARGS_BOX | URL_BOX | DIR_BOX if code == EN_KILLFOCUS => self.commit(id),
            OPEN if code == BN_CLICKED as u32 => {
                self.commit_all();
                if let Some(pet) = self.current_pet() {
                    tray::launch(&pet.id);
                }
            }
            HOME if code == BN_CLICKED as u32 => {
                if let Some(pet) = self.current_pet() {
                    tray::send_home(&pet.id);
                }
            }
            _ => {}
        }
    }

    fn link_clicked(&mut self, id: u16) {
        match id {
            LIKE_ALL => self.like_all(),
            RESET_APP => self.change(&[("app", None)]),
            RESET_URL => self.change(&[("url", None)]),
            RESET_PROGRAM => self.change(&[("program", None), ("args", None), ("shell", None)]),
            _ => {}
        }
    }
}

/// Whether the agent's hooks call this aipets.exe (plain text search in its config).
fn hook_text(pet: &PetInfo) -> (String, bool) {
    let exe = app::exe_path();
    let (file, marker) = match pet.status.as_str() {
        "claude" => (setup::claude_settings(), exe.replace('\\', "\\\\")),
        "hermes" => (win::combine(&setup::hermes_home(), "config.yaml"), exe.replace('\'', "''")),
        "codex" => (win::combine(&setup::codex_home(), "hooks.json"), exe.replace('\'', "''").replace('\\', "\\\\")),
        "cursor" => {
            let quoted = json::quote(&setup::cursor_command(exe));
            (win::combine(&setup::cursor_home(), "hooks.json"), quoted[1..quoted.len() - 1].to_string()) // as JSON writes it
        }
        _ => return ("–".into(), false),
    };
    let mut ok = false;
    let mut other = false; // aipets hooks, but for an exe somewhere else (folder moved or renamed)
    if let Ok(text) = win::read_text(&file) {
        // Cursor's hooks carry no arguments (the event comes with the payload)
        let hooked = (if pet.status == "cursor" { win::contains_ignore_case(&text, "aipets.exe") } else { text.contains("--hook") })
            && win::contains_ignore_case(&text, "aipets");
        ok = hooked && win::contains_ignore_case(&text, &marker);
        other = hooked && !ok;
    }
    let place = app::short_path(&file);
    if ok {
        (format!("✓ Hooks set up ({place})"), true)
    } else if other {
        ("Hooks call a different aipets.exe".into(), false)
    } else {
        (format!("No hooks in {place}"), false)
    }
}

// ------------------------------------------------------------------ painting (no window state needed)

fn paint(hwnd: HWND, dc: HDC) {
    let (pane, white, line, k) = look(|l| (l.pane, l.white, l.line, l.k));
    let mut rc: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut rc) };
    let bar_y = s(BAR_Y);
    fill(dc, Rect::new(0, 0, s(SIDE_W), bar_y), pane);
    fill(dc, Rect::new(s(SIDE_W), 0, rc.right - s(SIDE_W), bar_y), white);
    fill(dc, Rect::new(0, bar_y, rc.right, rc.bottom - bar_y), pane);
    unsafe {
        let old = SelectObject(dc, line as HGDIOBJ);
        MoveToEx(dc, 0, bar_y, null_mut());
        LineTo(dc, rc.right, bar_y);
        SelectObject(dc, old);
    }
    // the avatar: the largest whole-number scale that fits, without smoothing
    if let Some(info) = AVATAR.with(|a| a.borrow().clone()) {
        let bmp = pet_bitmap(&info, 48);
        let boxed = sr(PAGE_X + 22, 16, 96, 96);
        let scale = 1.max((boxed.w / 48).min(boxed.h / 48));
        let side = 48 * scale;
        let _ = k;
        draw_bitmap(dc, bmp, 48, Rect::new(boxed.x + (boxed.w - side) / 2, boxed.y + (boxed.h - side) / 2, side, side));
    }
}

fn draw_item(d: &DRAWITEMSTRUCT) {
    let Some(info) = PETS.with(|p| p.borrow().get(d.itemID as usize).cloned()) else { return };
    let (pane, selection, accent, name_font, font, k) = look(|l| (l.pane, l.selection, l.accent, l.name_font, l.font, l.k));
    let r = Rect::new(d.rcItem.left, d.rcItem.top, d.rcItem.right - d.rcItem.left, d.rcItem.bottom - d.rcItem.top);
    let dc = d.hDC;
    fill(dc, r, pane);
    if d.itemState & ODS_SELECTED != 0 {
        let card = Rect::new(r.x + 2, r.y + 3, r.w - 4, r.h - 6);
        let diameter = (12.0 * k).round() as i32;
        unsafe {
            let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
            let old_brush = SelectObject(dc, selection as HGDIOBJ);
            RoundRect(dc, card.x, card.y, card.right() + 1, card.bottom() + 1, diameter, diameter);
            SelectObject(dc, old_brush);
            SelectObject(dc, old_pen);
        }
        fill(dc, Rect::new(card.x, card.y + card.h / 4, (3.0 * k).round().max(3.0) as i32, card.h / 2), accent);
    }
    let icon = (32.0 * k).round() as i32;
    draw_bitmap(dc, pet_bitmap(&info, 32), 32, Rect::new(r.x + (14.0 * k) as i32, r.y + (r.h - icon) / 2, icon, icon));
    let text_x = r.x + (56.0 * k) as i32;
    text_out(dc, name_font, DARK, text_x, r.y + (10.0 * k) as i32, &info.name);
    let state = if SNAPSHOT.with(|c| c.get()) { "–".to_string() } else { tray::state_text(&info.id) };
    text_out(dc, font, MUTED, text_x, r.y + (31.0 * k) as i32, &state);
}

fn control_color(dc: HDC, control: HWND) -> LRESULT {
    let id = unsafe { GetDlgCtrlID(control) } as u16;
    let color = match id {
        STATE | APP_WHERE | BROWSER | LIKE_ALL => MUTED,
        APP_NAME => APP_NAME_COLOR.with(|c| c.get()),
        HOOKS => HOOKS_COLOR.with(|c| c.get()),
        L_SIZE | L_PET_SIZE | L_OPENS | L_PROGRAM | L_LINK | L_APP | L_CLI | L_BROWSER | L_ARGS | L_SHELL | L_FOLDER | L_STATUS => LABEL,
        _ => cref(0, 0, 0),
    };
    unsafe {
        SetTextColor(dc, color);
        SetBkMode(dc, TRANSPARENT as i32);
    }
    look(|l| if (AUTOSTART..=CLOSE).contains(&id) || id == LIST { l.pane } else { l.white }) as LRESULT
}

// ------------------------------------------------------------------ messages

impl Window for Settings {
    fn message(slot: &Slot<Self>, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                slot.with(|s| s.build(hwnd))?;
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_PAINT => {
                unsafe {
                    let mut ps: PAINTSTRUCT = std::mem::zeroed();
                    let dc = BeginPaint(hwnd, &mut ps);
                    paint(hwnd, dc);
                    EndPaint(hwnd, &ps);
                }
                Some(0)
            }
            WM_PRINTCLIENT => {
                paint(hwnd, wp as HDC);
                Some(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => Some(control_color(wp as HDC, lp as HWND)),
            WM_CTLCOLORLISTBOX => Some(look(|l| l.pane) as LRESULT),
            WM_MEASUREITEM => {
                let m = unsafe { &mut *(lp as *mut MEASUREITEMSTRUCT) };
                m.itemHeight = s(58) as u32;
                Some(1)
            }
            WM_DRAWITEM => {
                let d = unsafe { &*(lp as *const DRAWITEMSTRUCT) };
                if d.CtlID == LIST as u32 {
                    draw_item(d);
                }
                Some(1)
            }
            WM_HSCROLL => {
                slot.with(|s| s.slider_moved(lp as HWND));
                Some(0)
            }
            WM_TIMER if wp == SIZE_TIMER => {
                slot.with(|s| s.write_heights());
                Some(0)
            }
            WM_REFRESH => {
                slot.with(|s| s.refresh_states());
                Some(0)
            }
            WM_RESHOW => {
                slot.with(|s| s.show_current());
                Some(0)
            }
            WM_COMMAND => {
                let id = (wp & 0xFFFF) as u16;
                let code = ((wp >> 16) & 0xFFFF) as u32;
                match id {
                    2 | CLOSE => unsafe {
                        PostMessageW(hwnd, WM_CLOSE, 0, 0); // Esc or "Close"
                    },
                    PICK_APP if code == BN_CLICKED as u32 => browse_app(slot, hwnd),
                    BROWSE if code == BN_CLICKED as u32 => browse_folder(slot, hwnd),
                    _ => {
                        slot.with(|s| s.command(id, code));
                    }
                }
                Some(0)
            }
            WM_NOTIFY => {
                let hdr = unsafe { &*(lp as *const NMHDR) };
                if hdr.code == NM_CLICK || hdr.code == NM_RETURN {
                    match hdr.idFrom as u16 {
                        SETUP => setup_hooks(slot),
                        LOG => open_log(hwnd),
                        id => {
                            slot.with(|s| s.link_clicked(id));
                        }
                    }
                    return Some(0);
                }
                None
            }
            WM_CLOSE => {
                slot.with(|s| {
                    s.commit_all();
                    s.write_heights();
                    s.closing = true;
                });
                unsafe { DestroyWindow(hwnd) };
                Some(0)
            }
            WM_DESTROY => {
                if dialog_hwnd() == hwnd {
                    HWND_NOW.store(0, Ordering::Relaxed);
                }
                AVATAR.with(|a| *a.borrow_mut() = None);
                None
            }
            _ => None,
        }
    }
}

// handlers with a dialog: the window's state is not held while it is open

fn browse_app(slot: &Slot<Settings>, hwnd: HWND) {
    let Some(Some(pet)) = slot.with(|s| s.current_pet()) else { return };
    let saved = tray::settings_of(&pet.id).unwrap_or_default();
    let exe = DesktopApp::find(&saved.desktop_app).and_then(|a| a.exe);
    let (dir, file) = exe.map(|e| (win::dir_name(&e), win::file_name(&e).to_string())).unwrap_or_default();
    if let Some(path) = dialogs::open_exe(hwnd, &format!("Which app should {} open?", pet.name), &dir, &file) {
        tray::change_setting(&pet.id, &[("app", if path == pet.desktop_app { None } else { Some(&path) })]);
        slot.with(|s| s.show_current());
    }
}

fn browse_folder(slot: &Slot<Settings>, hwnd: HWND) {
    let Some(Some((name, dir))) = slot.with(|s| s.current.map(|i| (s.pets[i].name.clone(), wide_text(s.ctl(DIR_BOX))))) else { return };
    if let Some(path) = dialogs::browse_folder(hwnd, &format!("Which folder should {name} start in?"), &dir) {
        slot.with(|s| {
            set_text(s.ctl(DIR_BOX), &path);
            s.commit(DIR_BOX);
        });
    }
}

/// "Set up hooks": in the background (Codex' approval can take a few seconds), then the result.
fn setup_hooks(slot: &Slot<Settings>) {
    let Some(Some(pet)) = slot.with(|s| s.current_pet()) else { return };
    if SETTING_UP.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let step = setup::hooks(&pet.status, app::exe_path(), true);
        SETTING_UP.store(false, Ordering::Release);
        win::post(dialog_hwnd(), WM_RESHOW, 0, 0);
        tray::post(HostMsg::Alert { title: TITLE_TEXT.into(), text: step.to_string(), warn: !step.ok });
    });
}

fn open_log(hwnd: HWND) {
    let path = app::log_path().to_string_lossy().into_owned();
    let r = unsafe { ShellExecuteW(hwnd, win::wide("open").as_ptr(), win::wide(&path).as_ptr(), null(), null(), SW_SHOWNORMAL) } as isize;
    if r <= 32 {
        win::message_box(hwnd, &format!("The log could not be opened (error {r}).\n\n{path}"), TITLE_TEXT, MB_OK | MB_ICONWARNING);
    }
}

// ------------------------------------------------------------------ snapshot (--snapshot)

/// Layout check: renders the window (the page of pet_id, or the first pet) from an off-screen copy that is
/// never activated: settings.png as saved, settings-<mode>.png for each mode the pet offers.
pub fn snapshot(dir: &str, pet_id: Option<&str>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let hwnd = create(true);
    if hwnd.is_null() {
        return Err("the settings window could not be created".into());
    }
    unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    wnd::with::<Settings, _>(hwnd, |s| s.select_pet(pet_id));
    let result = (|| {
        save_png(hwnd, &win::combine(dir, "settings.png"))?;
        for mode in ["program", "app", "website"] {
            let Some(Some(info)) = wnd::with::<Settings, _>(hwnd, |s| s.current.map(|i| s.pets[i].clone())) else { break };
            let mut st = PetSettings::from(&info, &store::load());
            st.use_mode(mode);
            if st.mode != mode {
                continue;
            }
            wnd::with::<Settings, _>(hwnd, |s| {
                s.snapshot_mode = Some(mode.to_string());
                s.show_current();
            });
            save_png(hwnd, &win::combine(dir, &format!("settings-{mode}.png")))?;
        }
        Ok(())
    })();
    unsafe {
        let owner = GetWindow(hwnd, GW_OWNER);
        DestroyWindow(hwnd);
        if !owner.is_null() {
            DestroyWindow(owner);
        }
    }
    result
}

fn pump(hwnd: HWND) {
    unsafe {
        RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW);
        for _ in 0..3 {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }
}

fn save_png(hwnd: HWND, path: &str) -> Result<(), String> {
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    pump(hwnd);
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut r);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let screen = GetDC(null_mut());
        let mut bits: *mut std::ffi::c_void = null_mut();
        let dib = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        if dib.is_null() || dc.is_null() {
            return Err("no bitmap for the snapshot".into());
        }
        let old = SelectObject(dc, dib as HGDIOBJ);
        PrintWindow(hwnd, dc, 2); // PW_RENDERFULLCONTENT: what the screen would show
        let all = std::slice::from_raw_parts(bits as *const u32, (w * h) as usize);
        // only the visible frame: the invisible resize borders around it come out black
        let mut visible: RECT = std::mem::zeroed();
        let found = windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows_sys::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut visible as *mut RECT as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        ) == 0;
        let (left, top) = if found { ((visible.left - r.left).clamp(0, w), (visible.top - r.top).clamp(0, h)) } else { (0, 0) };
        let (right, bottom) = if found { ((visible.right - r.left).clamp(left, w), (visible.bottom - r.top).clamp(top, h)) } else { (w, h) };
        let (cw, ch) = (right - left, bottom - top);
        let mut px: Vec<atlas::Pixel> = Vec::with_capacity((cw * ch) as usize);
        for y in top..bottom {
            px.extend(all[(y * w + left) as usize..(y * w + right) as usize].iter().map(|p| p | 0xFF00_0000));
        }
        SelectObject(dc, old);
        DeleteObject(dib as HGDIOBJ);
        DeleteDC(dc);
        atlas::write_png(path, cw, ch, &px)
    }
}
