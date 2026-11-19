//! The part that runs in the background: tray icon with menu, the settings window and every pet, all in
//! this one process and on this one thread. The tray starts each shown pet as a window and starts her
//! again (with growing pauses) if she crashed. A hidden window "aipets.host" takes commands from other
//! processes (WM_COPYDATA) and runs work handed over from pets and worker threads (HostMsg).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, autostart, store, Ini};
use crate::atlas::round;
use crate::ipc;
use crate::launcher;
use crate::pet;
use crate::pets::{self, PetInfo, PetSettings, HEIGHT_UNIT, MIN_HEIGHT};
use crate::settings;
use crate::win::{self, NamedMutex, Rect};
use crate::win_com::ComInit;
use crate::wnd::{self, menu, Slot, Window};

pub const EXIT_ALREADY_RUNNING: i32 = 3;
const RETRY_MS: [i64; 6] = [1000, 2000, 5000, 15000, 30000, 60000];
const WM_RUN: u32 = WM_APP + 1;
const WM_TRAYICON: u32 = WM_APP + 2;
const SUPERVISE_TIMER: usize = 1;
const ICON_ID: u32 = 1;

// tray menu
const T_SETTINGS: u32 = 1;
const T_HIDE_ALL: u32 = 2;
const T_AUTOSTART: u32 = 3;
const T_QUIT: u32 = 4;
const T_PET: u32 = 100; // + 10 × pet index + 0 show, 1 open, 2 back to the corner

/// Work for the tray's thread, from pets, worker threads and other processes.
pub enum HostMsg {
    /// "settings [id]", "hide id", "quit"
    Command(String),
    /// A message box, e.g. a click that could not open anything.
    Alert { title: String, text: String, warn: bool },
    /// A pet's window went away after a panic.
    Crashed(String),
}

static QUEUE: Mutex<VecDeque<HostMsg>> = Mutex::new(VecDeque::new());
static HOST: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Hands work to the tray's thread (callable from any thread). Dropped when no tray runs (snapshots).
pub fn post(msg: HostMsg) {
    let host = host_hwnd();
    if host.is_null() {
        return;
    }
    if let Ok(mut q) = QUEUE.lock() {
        q.push_back(msg);
    }
    win::post(host, WM_RUN, 0, 0);
}

/// The hidden tray window (owner of menus), or null when no tray runs in this process.
pub fn host_hwnd() -> HWND {
    HOST.load(Ordering::Relaxed) as HWND
}

/// One pet as the tray sees it.
struct PetSlot {
    info: PetInfo,
    hwnd: HWND,
    lock: Option<NamedMutex>,
    stopping: bool,
    started_at: i64,
    retry_at: i64,
    stop_deadline: i64,
    failures: usize,
    problem: Option<String>,
}

impl PetSlot {
    fn running(&self) -> bool {
        win::is_window(self.hwnd)
    }
}

struct Tray {
    pets: Vec<PetSlot>,
    ini: Ini,
    ini_stamp: u64,
    ini_gen: u64,  // counts reloads: the settings window shows a page again when it changes
    ini_read: bool, // settings.ini was read once: before that no pet starts, a hidden one would show up
    quitting: bool,
    dry_run: bool,
    clock: Instant,
    icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    images: HashMap<String, HBITMAP>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// The tray's state, unless it is in use further up the stack (or no tray runs): then None.
fn with<R>(f: impl FnOnce(&mut Tray) -> R) -> Option<R> {
    TRAY.with(|t| t.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

impl Tray {
    fn new(dry_run: bool) -> Tray {
        let mut t = Tray {
            pets: PetInfo::discover()
                .into_iter()
                .map(|info| PetSlot {
                    info,
                    hwnd: std::ptr::null_mut(),
                    lock: None,
                    stopping: false,
                    started_at: 0,
                    retry_at: 0,
                    stop_deadline: 0,
                    failures: 0,
                    problem: None,
                })
                .collect(),
            ini: Ini::new(),
            ini_stamp: 0,
            ini_gen: 0,
            ini_read: false,
            quitting: false,
            dry_run,
            clock: Instant::now(),
            icon: std::ptr::null_mut(),
            images: HashMap::new(),
        };
        t.reload_ini();
        if !dry_run && t.ini_read {
            let infos: Vec<PetInfo> = t.pets.iter().map(|p| p.info.clone()).collect();
            store::change(|ini| migrate_sizes(ini, &infos));
            t.reload_ini();
            autostart::apply_default(&t.ini);
        }
        t
    }

    fn now(&self) -> i64 {
        self.clock.elapsed().as_millis() as i64
    }

    /// Unreadable settings (locked for a moment) keep the last ones; the next second tries again.
    fn reload_ini(&mut self) {
        let stamp = store::stamp();
        let Some(loaded) = store::try_load() else { return };
        self.ini_stamp = stamp;
        self.ini = loaded;
        self.ini_read = true;
        self.ini_gen += 1;
    }

    fn pet_mut(&mut self, id: &str) -> Option<&mut PetSlot> {
        self.pets.iter_mut().find(|p| win::eq_ignore_case(&p.info.id, id))
    }

    fn settings_of(&self, info: &PetInfo) -> PetSettings {
        PetSettings::from(info, &self.ini)
    }

    fn state_text(&self, p: &PetSlot) -> String {
        if !p.info.has_sprites() {
            return "sprites missing".into();
        }
        if !self.settings_of(&p.info).enabled {
            return "hidden".into();
        }
        if pets::hides_all(&self.ini) {
            return "all hidden".into();
        }
        if p.running() {
            return if p.stopping {
                "stopping …"
            } else if p.failures > 0 {
                "running again"
            } else {
                "running"
            }
            .into();
        }
        if p.failures > 0 {
            return "crashed, restarting …".into();
        }
        match &p.problem {
            Some(problem) => format!("Error: {problem}"),
            None => "starting …".into(),
        }
    }

    /// The pet's window went away while she should be shown: count it and wait a little longer each time.
    fn exited(&mut self, i: usize, now: i64, wanted: bool) {
        let p = &mut self.pets[i];
        p.hwnd = std::ptr::null_mut();
        p.lock = None;
        if p.stopping || !wanted {
            p.stopping = false;
            return;
        }
        p.failures = if now - p.started_at > 60000 { 1 } else { p.failures + 1 };
        p.retry_at = now + RETRY_MS[p.failures.min(RETRY_MS.len()) - 1];
        if p.problem.is_none() {
            p.problem = Some("crashed".into());
        }
        app::log(&format!("{} crashed, restart #{}", p.info.id, p.failures));
        if p.failures == 4 {
            let text = format!("{} keeps crashing. Details are in the log.", p.info.name);
            self.balloon(&text);
        }
    }

    fn pet_image(&mut self, info: &PetInfo, size: i32) -> HBITMAP {
        let key = format!("{}/{size}", info.id);
        *self.images.entry(key).or_insert_with(|| {
            let icon = win::load_icon_file(&info.icon_path(), size);
            let bmp = win::icon_bitmap(icon, size); // no icon.ico yet: menus simply show none
            if !icon.is_null() {
                unsafe { DestroyIcon(icon) };
            }
            bmp
        })
    }

    fn icon_data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = host_hwnd();
        data.uID = ICON_ID;
        data
    }

    fn add_icon(&self) {
        let mut data = self.icon_data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAYICON;
        data.hIcon = self.icon;
        copy_into(&mut data.szTip, app::NAME);
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
    }

    fn remove_icon(&self) {
        let data = self.icon_data();
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }

    fn balloon(&self, text: &str) {
        let mut data = self.icon_data();
        data.uFlags = NIF_INFO;
        copy_into(&mut data.szInfoTitle, app::NAME);
        copy_into(&mut data.szInfo, text);
        data.dwInfoFlags = NIIF_WARNING;
        data.Anonymous.uTimeout = 6000;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    fn build_menu(&mut self) -> HMENU {
        let m = menu::new();
        menu::item(m, T_SETTINGS, "Settings …", false, true);
        menu::bold(m, T_SETTINGS);
        menu::separator(m);
        let all_hidden = pets::hides_all(&self.ini);
        for i in 0..self.pets.len() {
            let info = self.pets[i].info.clone();
            let s = self.settings_of(&info);
            let sub = menu::new();
            let base = T_PET + 10 * i as u32;
            // her own choice waits until all pets are shown again
            menu::item(sub, base, "Show", s.enabled, !all_hidden);
            menu::item(sub, base + 1, &info.open_text, false, true);
            menu::item(sub, base + 2, "Back to the corner", false, s.shown());
            menu::submenu(m, &info.name, sub);
            let image = self.pet_image(&info, 16);
            menu::image(m, image);
        }
        if !self.pets.is_empty() {
            menu::separator(m);
            menu::item(m, T_HIDE_ALL, "Hide all pets", all_hidden, true);
        }
        menu::item(m, T_AUTOSTART, "Start with Windows", autostart::enabled(), true);
        menu::item(m, T_QUIT, "Quit", false, true);
        m
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        for (_, bmp) in self.images.drain() {
            if !bmp.is_null() {
                unsafe { DeleteObject(bmp as HGDIOBJ) };
            }
        }
    }
}

fn copy_into(dest: &mut [u16], text: &str) {
    for (i, c) in text.encode_utf16().take(dest.len() - 1).enumerate() {
        dest[i] = c;
    }
}

// ------------------------------------------------------------------ start, watch, stop

/// Every second, and after every change: starts shown pets, closes hidden ones, notices crashed ones.
pub fn supervise() {
    struct Plan {
        start: Vec<(usize, PetInfo)>,
        destroy: Vec<HWND>,
        dry_run: bool,
        now: i64,
    }
    let plan = with(|t| {
        if !t.ini_read || store::stamp() != t.ini_stamp {
            t.reload_ini();
        }
        let now = t.now();
        let mut plan = Plan { start: Vec::new(), destroy: Vec::new(), dry_run: t.dry_run, now };
        for i in 0..t.pets.len() {
            let wanted = t.ini_read && !t.quitting && t.pets[i].info.has_sprites() && t.settings_of(&t.pets[i].info).shown();
            if !t.pets[i].hwnd.is_null() && !t.pets[i].running() {
                t.exited(i, now, wanted);
            }
            let p = &mut t.pets[i];
            if p.hwnd.is_null() {
                if wanted && now >= p.retry_at {
                    // an older aipets (another process) may still show her: it closes once its tray is gone
                    match NamedMutex::create(&format!(r"Local\aipets.pet.{}", p.info.id), true) {
                        Some((lock, true)) => {
                            p.lock = Some(lock);
                            plan.start.push((i, p.info.clone()));
                        }
                        _ => p.retry_at = now + 2000,
                    }
                }
            } else if !wanted && !p.stopping {
                p.stopping = true;
                p.stop_deadline = now + 3000;
                if !win::post(p.hwnd, WM_CLOSE, 0, 0) {
                    plan.destroy.push(p.hwnd);
                }
            } else if p.stopping && now > p.stop_deadline {
                plan.destroy.push(p.hwnd);
            } else if p.failures > 0 && now - p.started_at > 60000 {
                p.failures = 0; // running steadily again
                p.problem = None;
            }
        }
        plan
    });
    let Some(plan) = plan else { return };
    // windows are made without holding the tray's state: creating one sends messages right away
    for (i, info) in plan.start {
        let result = pet::create(&info, plan.dry_run);
        with(|t| {
            let p = &mut t.pets[i];
            p.started_at = plan.now;
            p.stopping = false;
            match result {
                Ok(hwnd) => p.hwnd = hwnd,
                Err(e) => {
                    app::log(&format!("start {}: {e}", info.id));
                    p.lock = None;
                    p.failures += 1;
                    p.retry_at = plan.now + RETRY_MS[p.failures.min(RETRY_MS.len()) - 1];
                    p.problem = Some(e);
                }
            }
        });
    }
    for hwnd in plan.destroy {
        if win::is_window(hwnd) {
            unsafe { DestroyWindow(hwnd) };
        }
    }
    settings::refresh();
}

/// Older settings counted sizes in steps of 162 px: one per pet (scale), later one for all ([app] size) with a
/// share per pet (percent). Now they are heights in screen pixels: [app] height for all pets, [id] height for a
/// pet with her own. Runs once; the old keys go.
pub fn migrate_sizes(ini: &mut Ini, pets: &[PetInfo]) {
    if ini.get("app", "height").is_some() {
        return;
    }
    let mut shared = old_size(ini.get("app", "size"));
    if shared == 0.0 {
        shared = common_old_size(ini, pets);
    }
    if shared == 0.0 {
        return;
    }
    let all = to_height(shared);
    ini.set("app", "height", Some(&app::number(all)));
    ini.set("app", "size", None);
    for p in pets {
        let id = &p.id;
        if ini.get(id, "scale").is_none() && ini.get(id, "percent").is_none() {
            continue;
        }
        let own = to_height(own_old_size(ini, id, shared));
        ini.set(id, "scale", None);
        ini.set(id, "percent", None);
        if ini.get(id, "height").is_none() {
            let value = app::number(own);
            ini.set(id, "height", if own == all { None } else { Some(&value) });
        }
    }
    app::log(&format!("sizes now heights: {all} px for all pets"));
}

/// An old size in pixels, never below the smallest height (an old 50 % could mean 81 px).
fn to_height(old: f64) -> i32 {
    MIN_HEIGHT.max(round(old * HEIGHT_UNIT as f64))
}

/// An old size (a multiple of 162 px), or 0.
fn old_size(text: Option<&str>) -> f64 {
    match text.and_then(|t| t.trim().parse::<f64>().ok()) {
        Some(v) if v.is_finite() && v > 0.0 => v,
        _ => 0.0,
    }
}

/// A pet's old size: the shared one times her percent (last version), else her own scale (first versions), else the shared one.
fn own_old_size(ini: &Ini, id: &str, shared: f64) -> f64 {
    let percent = ini.get_int(id, "percent", 0);
    if percent > 0 {
        return shared * percent as f64 / 100.0;
    }
    let scale = old_size(ini.get(id, "scale"));
    if scale > 0.0 { scale } else { shared }
}

/// The most common per-pet size of the first versions (the larger on a tie), or 0.
fn common_old_size(ini: &Ini, pets: &[PetInfo]) -> f64 {
    let mut counts: Vec<(f64, i32)> = Vec::new();
    for p in pets {
        let old = old_size(ini.get(&p.id, "scale"));
        if old > 0.0 {
            match counts.iter_mut().find(|(k, _)| *k == old) {
                Some(entry) => entry.1 += 1,
                None => counts.push((old, 1)),
            }
        }
    }
    let mut best: Option<(f64, i32)> = None;
    for (size, count) in counts {
        if best.map_or(true, |(bs, bc)| count > bc || (count == bc && size > bs)) {
            best = Some((size, count));
        }
    }
    best.map(|b| b.0).unwrap_or(0.0)
}

// ------------------------------------------------------------------ what the settings window and pets use

pub fn pets() -> Vec<PetInfo> {
    with(|t| t.pets.iter().map(|p| p.info.clone()).collect()).unwrap_or_default()
}

/// The settings as the tray last read them, and how often it has read them.
pub fn ini() -> Option<(Ini, u64)> {
    with(|t| (t.ini.clone(), t.ini_gen))
}

pub fn ini_gen() -> u64 {
    with(|t| t.ini_gen).unwrap_or(0)
}

pub fn settings_of(id: &str) -> Option<PetSettings> {
    with(|t| t.pets.iter().find(|p| p.info.id == id).map(|p| t.settings_of(&p.info))).flatten()
}

pub fn state_text(id: &str) -> String {
    with(|t| t.pets.iter().find(|p| p.info.id == id).map(|p| t.state_text(p))).flatten().unwrap_or_else(|| "–".into())
}

pub fn set_enabled(id: &str, enabled: bool) {
    store::update(id, &[("enabled", if enabled { None } else { Some("0") })]);
    with(|t| {
        if let Some(p) = t.pet_mut(id) {
            p.failures = 0;
            p.retry_at = 0;
            p.problem = None;
        }
        t.reload_ini();
    });
    supervise();
}

/// Hides all pets or brings back the ones that were shown; each pet's own "Show" stays untouched.
pub fn set_all_hidden(hidden: bool) {
    store::update("app", &[("hidden", if hidden { Some("1") } else { None })]);
    with(|t| {
        for p in &mut t.pets {
            p.failures = 0;
            p.retry_at = 0;
            p.problem = None;
        }
        t.reload_ini();
    });
    supervise();
}

/// change_setting("claude", &[("args", Some("--yolo")), ("shell", None)]): None goes back to the pet.ini default.
pub fn change_setting(id: &str, values: &[(&str, Option<&str>)]) {
    store::update(id, values);
    with(|t| t.reload_ini());
    ipc::post_to_pet(id, ipc::CMD_RELOAD);
}

/// The slider for all pets: every pet snaps to this height right away, own heights go.
pub fn set_height(px: i32) {
    store::change(|ini| pets::share_height(ini, px));
    with(|t| t.reload_ini());
    for info in pets() {
        ipc::post_to_pet(&info.id, ipc::CMD_RELOAD);
    }
}

pub fn send_home(id: &str) {
    store::update(id, &[("x", None), ("y", None)]);
    with(|t| t.reload_ini());
    ipc::post_to_pet(id, ipc::CMD_RELOAD);
}

/// A visible pet opens it herself (with her little animation); a hidden one is launched from here.
pub fn launch(id: &str) {
    let Some(Some((info, s, running, dry_run))) = with(|t| {
        t.pets.iter().find(|p| p.info.id == id).map(|p| (p.info.clone(), t.settings_of(&p.info), p.running(), t.dry_run))
    }) else {
        return;
    };
    if running && ipc::post_to_pet(id, ipc::CMD_LAUNCH) {
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = launcher::launch(&info, &s, dry_run) {
            app::log(&format!("launch failed: {e}"));
            post(HostMsg::Alert { title: info.name.clone(), text: e, warn: true });
        }
    });
}

pub fn quit() {
    let Some(Some(windows)) = with(|t| {
        if t.quitting {
            return None;
        }
        t.quitting = true;
        Some(t.pets.iter().map(|p| p.hwnd).filter(|h| !h.is_null()).collect::<Vec<HWND>>())
    }) else {
        return;
    };
    app::log("tray quits");
    let host = host_hwnd();
    unsafe { KillTimer(host, SUPERVISE_TIMER) };
    settings::close();
    for hwnd in windows {
        if win::is_window(hwnd) {
            unsafe { DestroyWindow(hwnd) }; // each pet saves her position on the way
        }
    }
    with(|t| t.remove_icon());
    unsafe { DestroyWindow(host) };
}

fn run_command(text: &str) {
    let mut parts = text.splitn(2, ' ');
    let verb = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).filter(|a| !a.is_empty());
    match verb {
        "settings" => settings::open(arg),
        "hide" => {
            let id = arg.and_then(|a| with(|t| t.pet_mut(a).map(|p| p.info.id.clone())).flatten());
            if let Some(id) = id {
                set_enabled(&id, false);
            }
        }
        "quit" => quit(),
        _ => {}
    }
}

fn run_queue() {
    loop {
        let Some(msg) = QUEUE.lock().ok().and_then(|mut q| q.pop_front()) else { break };
        match msg {
            HostMsg::Command(text) => run_command(&text),
            HostMsg::Alert { title, text, warn } => {
                win::message_box(std::ptr::null_mut(), &text, &title, MB_OK | if warn { MB_ICONWARNING } else { MB_ICONINFORMATION });
            }
            HostMsg::Crashed(id) => {
                with(|t| {
                    if let Some(p) = t.pet_mut(&id) {
                        p.problem = Some("crashed (see the log)".into());
                    }
                });
                supervise();
            }
        }
    }
}

fn show_menu() {
    let Some(m) = with(|t| {
        t.reload_ini();
        t.build_menu()
    }) else {
        return;
    };
    let cmd = menu::track(m, host_hwnd(), &[T_SETTINGS]);
    match cmd {
        0 => {}
        T_SETTINGS => settings::open(None),
        T_HIDE_ALL => set_all_hidden(!with(|t| pets::hides_all(&t.ini)).unwrap_or(false)),
        T_AUTOSTART => {
            if let Err(e) = autostart::choose(!autostart::enabled()) {
                app::log(&format!("autostart: {e}"));
            }
        }
        T_QUIT => quit(),
        c if c >= T_PET => {
            let (index, action) = (((c - T_PET) / 10) as usize, (c - T_PET) % 10);
            let Some(Some((id, enabled))) =
                with(|t| t.pets.get(index).map(|p| (p.info.id.clone(), t.settings_of(&p.info).enabled)))
            else {
                return;
            };
            match action {
                0 => set_enabled(&id, !enabled),
                1 => launch(&id),
                2 => send_home(&id),
                _ => {}
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ host window and message loop

struct Host;

impl Window for Host {
    fn message(_: &Slot<Self>, _hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_COPYDATA => {
                // answer right away; a dialog opened here would block the sender
                let text = unsafe { ipc::read_copy_data(lp) };
                post(HostMsg::Command(text));
                Some(1)
            }
            WM_RUN => {
                run_queue();
                Some(0)
            }
            WM_TIMER if wp == SUPERVISE_TIMER => {
                supervise();
                Some(0)
            }
            WM_TRAYICON => {
                match (lp as u32) & 0xFFFF {
                    WM_LBUTTONUP => settings::open(None),
                    WM_RBUTTONUP => show_menu(),
                    _ => {}
                }
                Some(0)
            }
            WM_QUERYENDSESSION => {
                with(|t| t.quitting = true); // Windows closes the pets itself; don't bring them back
                Some(1)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                None
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                with(|t| t.add_icon()); // Explorer restarted: the icon is gone otherwise
                Some(0)
            }
            _ => None,
        }
    }
}

pub fn run(dry_run: bool) -> i32 {
    app::set_log_tag("tray");
    let Some((_single, first)) = NamedMutex::create(r"Local\aipets.tray", true) else { return 1 };
    if !first {
        // started again (double-click on the exe): open the running tray's settings instead
        ipc::send_to_host("settings");
        return EXIT_ALREADY_RUNNING;
    }
    let _com = ComInit::new(); // folder dialog, shell names, ShellExecute

    let mut tray = Tray::new(dry_run);
    let small = unsafe { GetSystemMetrics(SM_CXSMICON) };
    tray.icon = win::load_app_icon(small);
    let count = tray.pets.len();
    TRAY.with(|t| *t.borrow_mut() = Some(tray));

    wnd::register::<Host>("aipets.host", std::ptr::null(), std::ptr::null_mut());
    let host = wnd::create("aipets.host", ipc::HOST_TITLE, WS_POPUP, WS_EX_TOOLWINDOW, Rect::default(), std::ptr::null_mut(), Host);
    if host.is_null() {
        app::log("the tray window could not be created");
        return 1;
    }
    HOST.store(host as isize, Ordering::Relaxed);
    TASKBAR_CREATED.store(win::register_message("TaskbarCreated"), Ordering::Relaxed);
    with(|t| t.add_icon());
    unsafe { SetTimer(host, SUPERVISE_TIMER, 1000, None) };
    supervise();
    app::log(&format!("tray started with {count} pets{}", if dry_run { " (dry run)" } else { "" }));
    if count == 0 {
        with(|t| t.balloon(&format!("No pets found: {} is empty.", app::pets_dir())));
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            let dialog = settings::dialog_hwnd();
            if !dialog.is_null() && IsDialogMessageW(dialog, &msg) != 0 {
                continue; // Tab, Enter and Esc in the settings window
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    HOST.store(0, Ordering::Relaxed);
    TRAY.with(|t| t.borrow_mut().take());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pets(ids: &[&str]) -> Vec<PetInfo> {
        ids.iter().map(|id| PetInfo { id: id.to_string(), ..Default::default() }).collect()
    }

    #[test]
    fn sizes_become_heights() {
        // last version: [app] size with percent per pet
        let mut ini = Ini::parse("[app]\nsize=1\n[claude]\npercent=200\n[grok]\npercent=50\n[astra]\nx=5\n", "app");
        migrate_sizes(&mut ini, &pets(&["claude", "grok", "astra"]));
        assert_eq!(ini.get("app", "height"), Some("162"));
        assert_eq!(ini.get("app", "size"), None);
        assert_eq!(ini.get("claude", "height"), Some("324"));
        assert_eq!(ini.get("grok", "height"), None); // 81 px becomes 162 = like all
        assert_eq!(ini.get("grok", "percent"), None);
        // first versions: a scale per pet, the most common one (larger on a tie) for all
        let mut ini = Ini::parse("[claude]\nscale=2\n[hermes]\nscale=1\n", "app");
        migrate_sizes(&mut ini, &pets(&["claude", "hermes"]));
        assert_eq!(ini.get("app", "height"), Some("324"));
        assert_eq!(ini.get("hermes", "height"), Some("162"));
        // already heights: nothing changes
        let mut ini = Ini::parse("[app]\nheight=400\nsize=3\n", "app");
        migrate_sizes(&mut ini, &pets(&["claude"]));
        assert_eq!(ini.get("app", "size"), Some("3"));
        assert_eq!(old_size(Some(" 1.5 ")), 1.5);
        assert_eq!(old_size(Some("-1")), 0.0);
        assert_eq!(old_size(Some("NaN")), 0.0);
    }
}
