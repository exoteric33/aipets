//! Thin Win32 helpers shared by every part of aipets.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, GetFullPathNameW, FILE_ATTRIBUTE_DIRECTORY, INVALID_FILE_ATTRIBUTES};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock, ExpandEnvironmentStringsW};
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimePreciseAsFileTime, GetTickCount};
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, SHQueryUserNotificationState};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// A zero-terminated UTF-16 string from a pointer (null gives "").
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

impl Pt {
    pub fn new(x: i32, y: i32) -> Pt {
        Pt { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    fn from_rect(r: &RECT) -> Rect {
        Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{{X={},Y={},Width={},Height={}}}", self.x, self.y, self.w, self.h)
    }
}

// ------------------------------------------------------------------ folders and files

const FOLDERID_ROAMING_APP_DATA: GUID = GUID::from_u128(0x3eb685db_65f9_4cf6_a03a_e3ef65729f3d);
const FOLDERID_LOCAL_APP_DATA: GUID = GUID::from_u128(0xf1b32785_6fba_4fcf_9d55_7b8e7f157091);
const FOLDERID_PROFILE: GUID = GUID::from_u128(0x5e6c858f_0e22_4760_9afe_ea3317b67173);

fn known_folder(id: &GUID, env: &str) -> PathBuf {
    unsafe {
        let mut p: *mut u16 = null_mut();
        if SHGetKnownFolderPath(id, 0, null_mut(), &mut p) == 0 && !p.is_null() {
            let s = from_wide_ptr(p);
            CoTaskMemFree(p as *const c_void);
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        } else if !p.is_null() {
            CoTaskMemFree(p as *const c_void);
        }
    }
    PathBuf::from(std::env::var(env).unwrap_or_default())
}

pub fn roaming_app_data() -> PathBuf {
    known_folder(&FOLDERID_ROAMING_APP_DATA, "APPDATA")
}

pub fn local_app_data() -> PathBuf {
    known_folder(&FOLDERID_LOCAL_APP_DATA, "LOCALAPPDATA")
}

pub fn user_profile() -> PathBuf {
    known_folder(&FOLDERID_PROFILE, "USERPROFILE")
}

/// %VAR% expanded like Environment.ExpandEnvironmentVariables (unknown names stay as they are).
pub fn expand_env(text: &str) -> String {
    if !text.contains('%') {
        return text.to_string();
    }
    let src = wide(text);
    unsafe {
        let n = ExpandEnvironmentStringsW(src.as_ptr(), null_mut(), 0);
        if n == 0 {
            return text.to_string();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32);
        if got == 0 || got as usize > buf.len() {
            return text.to_string();
        }
        from_wide(&buf)
    }
}

/// File.Exists: an existing file (not a folder). Does not follow app execution aliases, so wt.exe counts.
pub fn file_exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let attrs = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_DIRECTORY == 0
}

pub fn dir_exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let attrs = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// Path.GetFullPath
pub fn full_path(path: &str) -> String {
    let src = wide(path);
    unsafe {
        let n = GetFullPathNameW(src.as_ptr(), 0, null_mut(), null_mut());
        if n == 0 {
            return path.to_string();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetFullPathNameW(src.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), null_mut());
        if got == 0 {
            return path.to_string();
        }
        from_wide(&buf)
    }
}

/// Path.GetFileName
pub fn file_name(path: &str) -> &str {
    match path.rfind(['\\', '/', ':']) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Path.GetFileNameWithoutExtension
pub fn file_stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// Path.HasExtension
pub fn has_extension(path: &str) -> bool {
    let name = file_name(path);
    matches!(name.rfind('.'), Some(i) if i + 1 < name.len())
}

/// Path.GetDirectoryName (simple form: everything before the last separator).
pub fn dir_name(path: &str) -> String {
    match path.rfind(['\\', '/']) {
        Some(0) => path[..1].to_string(),
        Some(i) if i == 2 && path.as_bytes().get(1) == Some(&b':') => path[..3].to_string(),
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// Path.Combine for two parts.
pub fn combine(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    if b.starts_with('\\') || b.starts_with('/') || b.as_bytes().get(1) == Some(&b':') {
        return b.to_string();
    }
    if a.ends_with('\\') || a.ends_with('/') || a.ends_with(':') {
        format!("{a}{b}")
    } else {
        format!("{a}\\{b}")
    }
}

/// Case-insensitive (ordinal, Unicode simple case folding) comparison.
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// Case-insensitive "contains".
pub fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

pub fn starts_with_ignore_case(hay: &str, prefix: &str) -> bool {
    hay.len() >= prefix.len() && hay.is_char_boundary(prefix.len()) && eq_ignore_case(&hay[..prefix.len()], prefix)
}

pub fn ends_with_ignore_case(hay: &str, suffix: &str) -> bool {
    hay.len() >= suffix.len()
        && hay.is_char_boundary(hay.len() - suffix.len())
        && eq_ignore_case(&hay[hay.len() - suffix.len()..], suffix)
}

/// File.ReadAllText: UTF-8 (a BOM is dropped) or UTF-16 with BOM.
pub fn read_text(path: &str) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(decode_text(&bytes))
}

pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    if bytes.len() >= 2 && (bytes[0] == 0xFF && bytes[1] == 0xFE || bytes[0] == 0xFE && bytes[1] == 0xFF) {
        let le = bytes[0] == 0xFF;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Last write time in 100 ns ticks since 1601 (0 if the file is missing).
pub fn file_stamp(path: &std::path::Path) -> u64 {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => system_time_ticks(t),
        Err(_) => 0,
    }
}

const TICKS_1601_TO_1970: u64 = 116_444_736_000_000_000;

pub fn system_time_ticks(t: std::time::SystemTime) -> u64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => TICKS_1601_TO_1970 + d.as_secs() * 10_000_000 + d.subsec_nanos() as u64 / 100,
        Err(e) => TICKS_1601_TO_1970.saturating_sub(e.duration().as_nanos() as u64 / 100),
    }
}

pub fn file_mtime_unix_ms(path: &str) -> i64 {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => ticks_to_unix_ms(system_time_ticks(t)),
        Err(_) => 0,
    }
}

pub fn ticks_to_unix_ms(ticks: u64) -> i64 {
    (ticks as i64 - TICKS_1601_TO_1970 as i64) / 10_000
}

pub fn unix_now_ms() -> i64 {
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimePreciseAsFileTime(&mut ft) };
    ticks_to_unix_ms(filetime_ticks(&ft))
}

pub fn now_ticks() -> u64 {
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimePreciseAsFileTime(&mut ft) };
    filetime_ticks(&ft)
}

pub fn filetime_ticks(ft: &FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

/// "yyyy-MM-ddTHH:mm:ss.ffffffZ" for 100 ns ticks since 1601 (UTC).
pub fn utc_iso(ticks: u64) -> String {
    let unix_ticks = ticks as i64 - TICKS_1601_TO_1970 as i64;
    let secs = unix_ticks.div_euclid(10_000_000);
    let micros = unix_ticks.rem_euclid(10_000_000) / 10;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{micros:06}Z", sod / 3600, sod / 60 % 60, sod % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// "yyyy-MM-dd HH:mm:ss" local time, for the log.
pub fn local_timestamp() -> String {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

// ------------------------------------------------------------------ mutex

/// A named Windows mutex (Local\...), shared with every aipets process.
pub struct NamedMutex(HANDLE);

impl NamedMutex {
    /// The mutex and whether this call created it (a C# "createdNew").
    pub fn create(name: &str, initial_owner: bool) -> Option<(NamedMutex, bool)> {
        unsafe {
            let h = CreateMutexW(null(), initial_owner as i32, wide(name).as_ptr());
            if h.is_null() {
                return None;
            }
            let created = GetLastError() != ERROR_ALREADY_EXISTS;
            Some((NamedMutex(h), created))
        }
    }

    /// Wait up to ms; an abandoned mutex counts as owned (its holder died, the data is whole or absent).
    pub fn wait(&self, ms: u32) -> bool {
        let r = unsafe { WaitForSingleObject(self.0, ms) };
        r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
    }

    pub fn release(&self) {
        unsafe { ReleaseMutex(self.0) };
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Runs f while holding the named mutex (waits up to ms); None if it timed out.
pub fn with_mutex<T>(name: &str, ms: u32, f: impl FnOnce() -> T) -> Option<T> {
    let (m, _) = NamedMutex::create(name, false)?;
    if !m.wait(ms) {
        return None;
    }
    struct Release<'a>(&'a NamedMutex);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _guard = Release(&m);
    Some(f())
}

// ------------------------------------------------------------------ registry

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn run_key_get(name: &str) -> Option<String> {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return None;
        }
        let wname = wide(name);
        let mut kind = 0u32;
        let mut size = 0u32;
        let mut result = None;
        if RegQueryValueExW(key, wname.as_ptr(), null(), &mut kind, null_mut(), &mut size) == 0
            && (kind == REG_SZ || kind == REG_EXPAND_SZ)
        {
            let mut buf = vec![0u16; size as usize / 2 + 1];
            let mut bytes = (buf.len() * 2) as u32;
            if RegQueryValueExW(key, wname.as_ptr(), null(), &mut kind, buf.as_mut_ptr() as *mut u8, &mut bytes) == 0 {
                result = Some(from_wide(&buf));
            }
        }
        RegCloseKey(key);
        result
    }
}

pub fn run_key_set(name: &str, value: Option<&str>) -> Result<(), String> {
    unsafe {
        let mut key: HKEY = null_mut();
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            null(),
            &mut key,
            null_mut(),
        );
        if r != 0 {
            return Err(format!("registry error {r}"));
        }
        let wname = wide(name);
        let r = match value {
            Some(v) => {
                let data = wide(v);
                RegSetValueExW(key, wname.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32)
            }
            None => {
                let r = RegDeleteValueW(key, wname.as_ptr());
                if r == ERROR_FILE_NOT_FOUND { 0 } else { r }
            }
        };
        RegCloseKey(key);
        if r == 0 { Ok(()) } else { Err(format!("registry error {r}")) }
    }
}

// ------------------------------------------------------------------ screens and input

fn monitor_info(m: HMONITOR) -> Option<(Rect, Rect)> {
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(m, &mut info) } == 0 {
        return None;
    }
    Some((Rect::from_rect(&info.rcMonitor), Rect::from_rect(&info.rcWork)))
}

/// Screen.FromPoint(p).WorkingArea
pub fn work_area_at(p: Pt) -> Rect {
    let m = unsafe { MonitorFromPoint(POINT { x: p.x, y: p.y }, MONITOR_DEFAULTTONEAREST) };
    monitor_info(m).map(|i| i.1).unwrap_or_else(primary_work_area)
}

/// Screen.PrimaryScreen.WorkingArea
pub fn primary_work_area() -> Rect {
    let m = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    monitor_info(m).map(|i| i.1).unwrap_or(Rect::new(0, 0, 1920, 1040))
}

/// Working areas of all screens.
pub fn all_work_areas() -> Vec<Rect> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data as *mut Vec<Rect>);
        if let Some((_, work)) = monitor_info(m) {
            list.push(work);
        }
        1
    }
    let mut list: Vec<Rect> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(each), &mut list as *mut Vec<Rect> as LPARAM) };
    if list.is_empty() {
        list.push(primary_work_area());
    }
    list
}

/// SystemInformation.VirtualScreen
pub fn virtual_screen() -> Rect {
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

pub fn drag_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) }
}

pub fn cursor_pos() -> Pt {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    Pt::new(p.x, p.y)
}

/// Milliseconds since the last keyboard or mouse input.
pub fn idle_ms() -> u32 {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return 0;
    }
    unsafe { GetTickCount() }.wrapping_sub(info.dwTime)
}

/// A fullscreen game, video or presentation is in front on the monitor of this window.
pub fn fullscreen_app_active(pet: HWND) -> bool {
    unsafe {
        let mut state = 0;
        if SHQueryUserNotificationState(&mut state) != 0 || !(state == 2 || state == 3 || state == 4) {
            return false; // only QUNS_BUSY, RUNNING_D3D_FULL_SCREEN, PRESENTATION_MODE can mean fullscreen
        }
        // "Show desktop" (Win+D) also reports QUNS_BUSY, with the desktop in front
        let fg = GetForegroundWindow();
        if fg.is_null() || IsIconic(fg) != 0 {
            return false;
        }
        let mut cls = [0u16; 64];
        GetClassNameW(fg, cls.as_mut_ptr(), cls.len() as i32);
        let cls = from_wide(&cls);
        if matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return false;
        }
        let monitor = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if monitor != MonitorFromWindow(pet, MONITOR_DEFAULTTONEAREST) {
            return false; // fullscreen on another screen: stay where she is
        }
        if state != 2 {
            return true;
        }
        let mut r: RECT = std::mem::zeroed();
        let Some((mon, _)) = monitor_info(monitor) else { return false };
        if GetWindowRect(fg, &mut r) == 0 {
            return false;
        }
        // maximised windows stop at the taskbar; fullscreen ones cover the whole monitor
        r.left <= mon.x && r.top <= mon.y && r.right >= mon.right() && r.bottom >= mon.bottom()
    }
}

/// Display DPI of the system (what WinForms reported as Graphics.DpiX).
pub fn system_dpi() -> u32 {
    let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() };
    if dpi == 0 { 96 } else { dpi }
}

// ------------------------------------------------------------------ environment

/// The user's environment as Windows builds it at logon (registry, not this process), so a terminal
/// opened by a pet never inherits variables of whoever started aipets (e.g. a Claude Code session).
pub fn logon_environment() -> Option<Vec<(String, String)>> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | windows_sys::Win32::Security::TOKEN_DUPLICATE, &mut token) == 0 {
            return None;
        }
        let mut block: *mut c_void = null_mut();
        let ok = CreateEnvironmentBlock(&mut block, token, 0) != 0;
        CloseHandle(token);
        if !ok || block.is_null() {
            return None;
        }
        let mut vars: Vec<(String, String)> = Vec::new();
        let mut p = block as *const u16;
        loop {
            let entry = from_wide_ptr(p);
            if entry.is_empty() {
                break;
            }
            let len = entry.encode_utf16().count();
            if let Some(eq) = entry[1.min(entry.len())..].find('=').map(|i| i + 1.min(entry.len())) {
                let name = entry[..eq].to_string();
                let value = entry[eq + 1..].to_string();
                if let Some(slot) = vars.iter_mut().find(|(k, _)| eq_ignore_case(k, &name)) {
                    slot.1 = value;
                } else {
                    vars.push((name, value));
                }
            }
            p = p.add(len + 1);
        }
        DestroyEnvironmentBlock(block);
        Some(vars)
    }
}

pub fn env_lookup<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| eq_ignore_case(k, name)).map(|(_, v)| v.as_str())
}

// ------------------------------------------------------------------ windows and messages

pub fn find_window(title: &str) -> HWND {
    unsafe { FindWindowW(null(), wide(title).as_ptr()) }
}

pub fn post(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> bool {
    !hwnd.is_null() && unsafe { PostMessageW(hwnd, msg, wp, lp) } != 0
}

pub fn is_window(hwnd: HWND) -> bool {
    !hwnd.is_null() && unsafe { IsWindow(hwnd) } != 0
}

pub fn register_message(name: &str) -> u32 {
    unsafe { RegisterWindowMessageW(wide(name).as_ptr()) }
}

pub fn message_box(owner: HWND, text: &str, title: &str, flags: u32) -> i32 {
    unsafe {
        MessageBoxW(owner, wide(text).as_ptr(), wide(title).as_ptr(), flags | MB_SETFOREGROUND | MB_TOPMOST)
    }
}

pub fn allow_any_foreground() {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(ASFW_ANY) };
}

/// Diagnostics commands of a GUI exe: print into the terminal that started it, unless the output is redirected.
pub fn attach_console() {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if (h.is_null() || h == INVALID_HANDLE_VALUE) && AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let out = GetStdHandle(STD_OUTPUT_HANDLE);
            if out.is_null() || out == INVALID_HANDLE_VALUE {
                use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING};
                let con = CreateFileW(wide("CONOUT$").as_ptr(), GENERIC_WRITE, FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut());
                if con != INVALID_HANDLE_VALUE {
                    windows_sys::Win32::System::Console::SetStdHandle(STD_OUTPUT_HANDLE, con);
                }
            }
        }
    }
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

// ------------------------------------------------------------------ fonts and icons

pub fn create_font(face: &str, points: f64, weight: i32, dpi: u32) -> HFONT {
    let height = -((points * dpi as f64 / 72.0).round() as i32);
    let mut name = [0u16; 32];
    for (i, c) in face.encode_utf16().take(31).enumerate() {
        name[i] = c;
    }
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            name.as_ptr(),
        )
    }
}

/// An icon file at one size (the best entry Windows finds for it), or null.
pub fn load_icon_file(path: &str, size: i32) -> HICON {
    unsafe { LoadImageW(null_mut(), wide(path).as_ptr(), IMAGE_ICON, size, size, LR_LOADFROMFILE) as HICON }
}

/// The icon embedded in aipets.exe (resource 1).
pub fn load_app_icon(size: i32) -> HICON {
    unsafe {
        let module = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
        let h = LoadImageW(module, 1 as *const u16, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as HICON;
        if h.is_null() { LoadIconW(null_mut(), IDI_APPLICATION) } else { h }
    }
}

/// A 32-bit premultiplied bitmap of an icon, for menu items.
pub fn icon_bitmap(icon: HICON, size: i32) -> HBITMAP {
    if icon.is_null() {
        return null_mut();
    }
    unsafe {
        let mut header: BITMAPINFO = std::mem::zeroed();
        header.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        header.bmiHeader.biWidth = size;
        header.bmiHeader.biHeight = -size;
        header.bmiHeader.biPlanes = 1;
        header.bmiHeader.biBitCount = 32;
        let screen = GetDC(null_mut());
        let mut bits: *mut c_void = null_mut();
        let bmp = CreateDIBSection(screen, &header, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        if bmp.is_null() || dc.is_null() {
            return null_mut();
        }
        let old = SelectObject(dc, bmp as HGDIOBJ);
        DrawIconEx(dc, 0, 0, icon, size, size, 0, null_mut(), DI_NORMAL);
        SelectObject(dc, old);
        DeleteDC(dc);
        bmp
    }
}

