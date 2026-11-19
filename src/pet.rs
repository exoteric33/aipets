//! One pet on the desktop. What she can do comes from her atlas: faces (normal, blink, happy and
//! optionally hover, look, sleep, drag), idle phases, bounce frames, particle anims.
//! All pets live in the tray's process and thread; each is a layered window with its own timer.

use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, store};
use crate::atlas::{self, round, Atlas, Canvas, Pixel, Surface};
use crate::dialogs;
use crate::ipc;
use crate::launcher::{self, DesktopApp};
use crate::layers;
use crate::pets::{self, PetInfo, PetSettings, HEIGHT_UNIT, MIN_HEIGHT};
use crate::status::{AgentState, StatusMonitor};
use crate::tray::{self, HostMsg};
use crate::win::{self, Pt, Rect};
use crate::wnd::{self, menu, Slot, Window};

// room around the character cell for the speech bubble and particles (cell pixels)
const PAD_X: i32 = 26;
const PAD_TOP: i32 = 20;
const IDLE_STEP_MS: i64 = 260;
const SLEEP_STEP_MS: i64 = 620;
const SLEEP_AFTER_MS: u32 = 60 * 1000;
const BUBBLE_DELAY_MS: i64 = 120;

const BOUNCE_STEPS: [i32; 9] = [-1, -2, -1, 1, 2, 3, 3, 2, 1];
const BOUNCE_MS: [i64; 9] = [40, 60, 40, 40, 40, 60, 60, 50, 50];
const SPARK_FRAMES: [&str; 6] = ["spark1", "spark2", "spark3", "spark2", "spark1", "spark0"];
const Z_FRAMES: [&str; 3] = ["z0", "z1", "z2"];
const PULSE_SPINNER: [&str; 8] = ["spin0", "spin1", "spin2", "spin3", "spin4", "spin3", "spin2", "spin1"];

const CLASS: &str = "aipets.pet";
const TIMER: usize = 1;

// right-click menu
const M_OPEN: u32 = 1;
const M_FOLDER: u32 = 2;
const M_WEBSITE: u32 = 3;
const M_APP: u32 = 4;
const M_PROGRAM: u32 = 5;
const M_SHARED: u32 = 10; // + 1 … 4: 162 … 648 px for all pets
const M_LIKE_ALL: u32 = 20;
const M_SMALLER: u32 = 21;
const M_BIGGER: u32 = 22;
const M_SCREEN: u32 = 23;
const M_HOME: u32 = 30;
const M_HIDE: u32 = 31;
const M_SETTINGS: u32 = 40;
const M_QUIT: u32 = 41;

type Frames = Rc<Vec<String>>;

struct Particle {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    gravity: f64,
    born: i64,
    life: i64,
    frames: Frames,
}

/// xorshift64*: blinks and sparkles need no more.
struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        static SEQ: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let seed = win::now_ticks() ^ SEQ.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed).rotate_left(17);
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// lo ≤ n < hi (Random.Next(lo, hi))
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }

    fn below(&mut self, n: i64) -> i64 {
        self.range(0, n)
    }

    fn double(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub struct Pet {
    info: PetInfo,
    hwnd: HWND,
    dry_run: bool,
    atlas: Atlas,
    clock: Instant,
    rng: Rng,
    particles: Vec<Particle>,
    all_pets: Vec<PetInfo>,     // every pet, for the stacking order and messages
    layer_order: Vec<String>,   // pet ids front to back (layers::order)
    own_processes: Vec<u32>,    // aipets' own windows (menus, dialogs) stay above every pet
    crashed: bool,

    // what this atlas offers
    can_bounce: bool,
    has_hover: bool,
    has_look: bool,
    has_sleep: bool,
    has_drag: bool,
    burst_frames: Frames,
    z_frames: Frames,
    spin_frames: Frames,
    twinkles: Vec<Frames>,

    settings: PetSettings,
    settings_stamp: u64,
    surface: Option<Surface>,
    height: i32, // how tall the figure is on screen, whatever her sprite height
    zoom: f64,   // screen pixels per sprite pixel at that height (rarely a whole number)
    anchor: Pt,  // bottom-centre of the pet on screen
    mirrored: bool,
    last_render: Option<String>,
    timer_ms: u32,

    now: i64,
    phase: i32,
    phase_at: i64,
    blink_until: i64,
    next_blink: i64,
    second_blink_at: i64,
    happy_until: i64,
    smile_until: i64,
    look_until: i64,
    bounce_at: i64,
    next_idle_action: i64,
    next_z: i64,
    hover_since: i64,
    next_fullscreen_check: i64,
    next_housekeeping: i64,
    next_restack: i64, // unix ms (layers::next_check): the pets take turns
    hint_until: i64,   // show the prompt bubble once after start
    last_launch: i64,
    sleeping: bool,
    hovered: bool,
    menu_open: bool,
    hidden_for_fullscreen: bool,
    pressed: bool,
    dragging: bool,
    press_cursor: Pt,
    press_anchor: Pt,

    // agent activity reported by hooks (see status.rs); done times are unix ms
    status: Option<StatusMonitor>,
    agent: AgentState,
    next_status_scan: i64,
    next_status_fx: i64,
    done_shown_since: i64,
    done_input_since: i64,
    done_acknowledged: i64,
}

/// Starts a pet: her window on the desktop, in her layer, animating. The tray watches it.
pub fn create(info: &PetInfo, dry_run: bool) -> Result<HWND, String> {
    wnd::register::<Pet>(CLASS, IDC_HAND, std::ptr::null_mut());
    let pet = Pet::new(info.clone(), dry_run)?;
    let r = pet.window_rect();
    let hwnd = wnd::create(
        CLASS,
        &ipc::pet_title(&info.id),
        WS_POPUP,
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        r,
        std::ptr::null_mut(),
        pet,
    );
    if hwnd.is_null() {
        return Err(format!("the window could not be created (error {})", std::io::Error::last_os_error()));
    }
    unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    Ok(hwnd)
}

impl Pet {
    fn new(info: PetInfo, dry_run: bool) -> Result<Pet, String> {
        let settings_stamp = store::stamp();
        let initial = store::try_load().ok_or("The pet settings cannot be read right now.")?;
        let settings = PetSettings::from(&info, &initial);
        let atlas = Atlas::load(&info.sprites_dir())?;
        let anim = |name: &str, fallback: &[&str]| Rc::new(atlas.anim(name, fallback));
        let burst_frames = anim("burst", &SPARK_FRAMES);
        let z_frames = anim("z", &Z_FRAMES);
        let spin_frames = anim("spin", &PULSE_SPINNER);
        let mut twinkles: Vec<Frames> = atlas.anims_with_prefix("twinkle").into_iter().map(Rc::new).collect();
        if twinkles.is_empty() {
            twinkles.push(Rc::new(SPARK_FRAMES.iter().map(|s| s.to_string()).collect()));
        }
        let status = if info.status.is_empty() { None } else { Some(StatusMonitor::new(&info.status, true)) };
        let all_pets = PetInfo::discover();
        let layer_order = layers::order(&initial, &all_pets);
        let mut pet = Pet {
            can_bounce: atlas.has_frame("bounce_1"),
            has_hover: atlas.has_frame("hover_0"),
            has_look: atlas.has_frame("look_0"),
            has_sleep: atlas.has_frame("sleep_0"),
            has_drag: atlas.has_frame("drag_0"),
            burst_frames,
            z_frames,
            spin_frames,
            twinkles,
            atlas,
            info,
            hwnd: std::ptr::null_mut(),
            dry_run,
            clock: Instant::now(),
            rng: Rng::new(),
            particles: Vec::new(),
            all_pets,
            layer_order,
            own_processes: vec![win::current_pid()],
            crashed: false,
            settings,
            settings_stamp,
            surface: None,
            height: 1,
            zoom: 1.0,
            anchor: Pt::default(),
            mirrored: false,
            last_render: None,
            timer_ms: 33,
            now: 0,
            phase: 0,
            phase_at: 0,
            blink_until: 0,
            next_blink: 3200,
            second_blink_at: -1,
            happy_until: 2800,
            smile_until: 0,
            look_until: 0,
            bounce_at: -1,
            next_idle_action: 9000,
            next_z: 0,
            hover_since: 0,
            next_fullscreen_check: 0,
            next_housekeeping: 0,
            next_restack: 0,
            hint_until: 2800,
            last_launch: i64::MIN / 2,
            sleeping: false,
            hovered: false,
            menu_open: false,
            hidden_for_fullscreen: false,
            pressed: false,
            dragging: false,
            press_cursor: Pt::default(),
            press_anchor: Pt::default(),
            status,
            agent: AgentState::Idle,
            next_status_scan: 0,
            next_status_fx: 0,
            done_shown_since: 0,
            done_input_since: 0,
            done_acknowledged: win::unix_now_ms(),
        };
        let saved = Pt::new(pet.settings.x, pet.settings.y);
        let start = if pet.settings.has_position { saved } else { Pt::new(win::primary_work_area().x, win::primary_work_area().y) };
        pet.set_height(fit_screen(start, pet.settings.pet_height()));
        pet.anchor = if pet.settings.has_position { pet.clamp(saved, true) } else { pet.home_anchor() };
        pet.mirrored = pet.wants_mirror(pet.anchor, false);
        pet.log(&format!(
            "start at {},{} (home {}, height {}, zoom {}, work area {})",
            pet.anchor.x,
            pet.anchor.y,
            pet.home_anchor().x,
            pet.height,
            three_decimals(pet.zoom),
            win::primary_work_area()
        ));
        Ok(pet)
    }

    fn log(&self, message: &str) {
        app::log_as(&self.info.id, message);
    }

    // ------------------------------------------------------------------ geometry

    /// Screen pixels for a length in cell pixels.
    fn px(&self, cell_pixels: f64) -> i32 {
        round(cell_pixels * self.zoom)
    }

    // the cell and the room around it, each rounded once, so the figure ends exactly at the window's bottom edge
    fn cell_px(&self) -> Rect {
        Rect::new(self.px(PAD_X as f64), self.px(PAD_TOP as f64), self.px(self.atlas.cell_w as f64), self.px(self.atlas.cell_h as f64))
    }

    fn canvas_px_w(&self) -> i32 {
        self.px(self.atlas.cell_w as f64) + 2 * self.px(PAD_X as f64)
    }

    fn canvas_px_h(&self) -> i32 {
        self.px(self.atlas.cell_h as f64) + self.px(PAD_TOP as f64)
    }

    /// She currently looks to the left (art direction combined with mirroring).
    fn looks_left(&self) -> bool {
        self.atlas.facing_left != self.mirrored
    }

    fn window_rect(&self) -> Rect {
        let (w, h) = (self.canvas_px_w(), self.canvas_px_h());
        Rect::new(self.anchor.x - w / 2, self.anchor.y - h, w, h)
    }

    /// "Back to the corner": pet.ini home is the gap from the screen's right edge to the cell's, in pixels
    /// at 162 px height. It grows with the height of all pets only, so the places stay put when one pet has her own.
    fn home_anchor(&self) -> Pt {
        let wa = win::primary_work_area();
        let gap = round(self.info.home as f64 * self.settings.shared_height() as f64 / HEIGHT_UNIT as f64);
        Pt::new(wa.right() - self.px(self.atlas.cell_w as f64) / 2 - gap, wa.bottom())
    }

    fn clamp(&self, mut p: Pt, snap_to_taskbar: bool) -> Pt {
        let vs = win::virtual_screen();
        let half = self.px((self.atlas.cell_w / 2 - 6) as f64);
        p.x = (vs.x + half).max((vs.right() - half).min(p.x));
        let wa = win::work_area_at(Pt::new(p.x, p.y.min(vs.bottom() - 1)));
        let min_y = vs.y + self.px((self.atlas.cell_h - 4) as f64); // keep the top of her head on screen
        p.y = min_y.max(wa.bottom().min(p.y));
        if snap_to_taskbar && wa.bottom() - p.y < self.px(18.0) {
            p.y = wa.bottom(); // she sits / stands on the taskbar edge
        }
        p
    }

    /// Mirror the frames so she looks towards the middle of the screen she is on.
    fn wants_mirror(&self, p: Pt, currently_mirrored: bool) -> bool {
        let wa = win::work_area_at(p);
        let mid = wa.x + wa.w / 2;
        let hysteresis = self.px(12.0);
        let look_left = if self.atlas.facing_left != currently_mirrored { p.x > mid - hysteresis } else { p.x > mid + hysteresis };
        look_left != self.atlas.facing_left
    }

    /// Height and zoom: the figure is px screen pixels tall, whatever her sprite height.
    fn set_height(&mut self, px: i32) {
        self.height = px;
        self.zoom = px as f64 / self.atlas.figure_height as f64;
    }

    fn screen_height(&self) -> i32 {
        win::work_area_at(self.anchor).h
    }

    fn apply_height(&mut self, px: i32) {
        self.set_height(px);
        self.anchor = self.clamp(self.anchor, false);
        self.surface = None;
        self.surface = Surface::new(self.canvas_px_w(), self.canvas_px_h());
        self.last_render = None;
        self.render(true);
    }

    fn move_to(&mut self, p: Pt, snap: bool) {
        self.anchor = self.clamp(p, snap);
        self.mirrored = self.wants_mirror(self.anchor, self.mirrored);
        self.render(true);
    }

    fn on_display_changed(&mut self) {
        self.fit_height(); // the screen may be lower now
        let target = if self.settings.has_position { self.anchor } else { self.home_anchor() };
        self.move_to(target, true);
    }

    /// After a move or a display change: the height she asks for, as far as her screen allows.
    fn fit_height(&mut self) {
        let fitted = fit_screen(self.anchor, self.settings.pet_height());
        if fitted != self.height {
            self.apply_height(fitted);
        }
    }

    /// settings.ini changed (settings window, "back to the corner", another size).
    fn reload_settings(&mut self) {
        let stamp = store::stamp();
        let Some(loaded) = store::try_load() else { return };
        self.settings_stamp = stamp;
        let s = PetSettings::from(&self.info, &loaded);
        let order = layers::order(&loaded, &self.all_pets);
        let reordered = order != self.layer_order;
        self.layer_order = order;
        if reordered && !self.hwnd.is_null() && !self.hidden_for_fullscreen {
            // another pet was dragged to the front: take the new place at once, with a new turn on the grid
            self.next_restack = layers::next_check(win::unix_now_ms(), self.rank());
            self.restack();
        }
        let new_height = fit_screen(self.anchor, s.pet_height());
        self.settings = s;
        if new_height != self.height {
            self.apply_height(new_height);
            if !self.settings.has_position {
                self.move_to(self.home_anchor(), true);
            }
            self.save_position();
            return;
        }
        if self.dragging {
            return;
        }
        if !self.settings.has_position {
            self.move_to(self.home_anchor(), true);
            self.save_position();
        } else if self.settings.x != self.anchor.x || self.settings.y != self.anchor.y {
            self.move_to(Pt::new(self.settings.x, self.settings.y), true);
        }
    }

    fn rank(&self) -> i64 {
        self.layer_order.iter().position(|x| *x == self.info.id).map(|i| i as i64).unwrap_or(-1)
    }

    // ------------------------------------------------------------------ animation

    fn on_created(&mut self, hwnd: HWND) {
        self.hwnd = hwnd;
        self.apply_height(self.height);
        self.restack(); // a new window starts on top of everything: into her layer before she shows up
        unsafe { SetTimer(hwnd, TIMER, self.timer_ms, None) };
    }

    fn tick(&mut self) {
        self.now = self.clock.elapsed().as_millis() as i64;
        let now = self.now;
        if now >= self.next_housekeeping {
            self.next_housekeeping = now + 1000;
            if store::stamp() != self.settings_stamp {
                self.reload_settings();
            }
        }
        if self.status.is_some() && now >= self.next_status_scan {
            self.update_agent_state();
            self.next_status_scan = now + 500;
        }
        self.update_hover();
        self.update_sleep();

        if now - self.phase_at >= if self.sleeping { SLEEP_STEP_MS } else { IDLE_STEP_MS } {
            self.phase = (self.phase + 1) % self.atlas.phases;
            self.phase_at = now;
        }
        if !self.sleeping && now >= self.next_blink {
            self.blink_until = now + 130;
            self.next_blink = now + self.rng.range(2200, 5600);
            if self.rng.below(4) == 0 {
                self.second_blink_at = now + 260;
            }
        }
        if self.second_blink_at >= 0 && now >= self.second_blink_at {
            self.blink_until = now + 120;
            self.second_blink_at = -1;
        }
        if !self.sleeping && !self.hovered && !self.dragging && now >= self.next_idle_action {
            self.idle_action();
            self.next_idle_action = now + self.rng.range(7000, 16000);
        }
        if self.sleeping && now >= self.next_z {
            self.spawn_z();
            self.next_z = now + 1300;
        }
        if !self.sleeping && !self.dragging && now >= self.next_status_fx {
            if self.agent == AgentState::Working {
                self.spawn_twinkle(); // thinking sparkles / music while the agent works
                self.next_status_fx = now + 2600;
            } else if self.agent == AgentState::Waiting {
                self.call_for_attention();
                self.next_status_fx = now + 3500;
            }
        }
        if now >= self.next_fullscreen_check {
            // a fullscreen video, game or presentation: step aside instead of floating over it
            self.hidden_for_fullscreen = !self.dragging && win::fullscreen_app_active(self.hwnd);
            self.next_fullscreen_check = now + 1000;
        }
        let wall = win::unix_now_ms();
        if !self.hidden_for_fullscreen && wall >= self.next_restack {
            self.next_restack = layers::next_check(wall, self.rank());
            self.restack();
        }
        self.particles.retain(|p| now - p.born < p.life);
        self.render(false);

        // fast ticks only while something moves quickly; idling stays cheap
        let interval = if !self.particles.is_empty() || self.bounce_at >= 0 || self.pressed {
            33
        } else if self.agent == AgentState::Working {
            55
        } else {
            80
        };
        if interval != self.timer_ms {
            self.timer_ms = interval;
            unsafe { SetTimer(self.hwnd, TIMER, interval, None) };
        }
    }

    /// Her fixed layer: above other programs' topmost windows, below the pets in front of her and below
    /// every menu and dialog of aipets (see layers.rs).
    fn restack(&mut self) {
        if self.hwnd.is_null() {
            return;
        }
        if !layers::restack(self.hwnd, &self.info.id, &self.layer_order, &self.own_processes) {
            self.next_restack = win::unix_now_ms() + layers::RETRY_MS; // windows were still moving: again shortly
        }
    }

    fn update_hover(&mut self) {
        let win = self.window_rect();
        let c = win::cursor_pos();
        let mut over = false;
        if win.contains(c.x, c.y) && !self.hidden_for_fullscreen {
            // back to cell pixels, the way compose stretches the cell
            let cell = self.cell_px();
            let cx = ((c.x - win.x - cell.x) as f64 * self.atlas.cell_w as f64 / cell.w as f64).floor() as i32;
            let cy = ((c.y - win.y - cell.y) as f64 * self.atlas.cell_h as f64 / cell.h as f64).floor() as i32;
            over = self.atlas.hit_character(cx, cy, self.mirrored)
                || (self.current_bubble().is_some() && self.bubble_rect().contains(cx + PAD_X, cy + PAD_TOP));
        }
        if over && !self.hovered {
            self.hover_since = self.now;
        }
        self.hovered = over;
    }

    /// Spinner while the agent works, "?" while it waits for you, a check mark when it is done.
    fn update_agent_state(&mut self) {
        let previous = self.agent;
        let Some(status) = self.status.as_mut() else { return };
        status.scan();
        self.agent = status.state;
        let latest_done = status.latest_done;
        if self.agent == AgentState::Waiting && previous != AgentState::Waiting {
            self.call_for_attention();
            self.next_status_fx = self.now + 3500;
        }

        let wall = win::unix_now_ms();
        if latest_done > self.done_acknowledged && latest_done > self.done_shown_since {
            self.done_shown_since = latest_done;
            self.done_input_since = 0;
            if self.agent == AgentState::Idle {
                self.cheer();
            }
        }
        if self.done_shown_since > 0 && wall - self.done_shown_since > 5000 {
            // the check mark stays until you have been back at the PC for a few seconds
            let last_input = wall - win::idle_ms() as i64;
            if last_input > self.done_shown_since + 5000 && self.done_input_since == 0 {
                self.done_input_since = wall;
            }
            if self.done_input_since > 0 && wall - self.done_input_since > 3000 {
                self.acknowledge_done();
            }
        }
    }

    fn acknowledge_done(&mut self) {
        self.done_acknowledged = self.done_acknowledged.max(self.done_shown_since);
        self.done_shown_since = 0;
        self.done_input_since = 0;
    }

    fn update_sleep(&mut self) {
        let idle = win::idle_ms();
        let agent_needs_her = self.agent != AgentState::Idle || self.done_shown_since > 0;
        if !self.sleeping && idle > SLEEP_AFTER_MS && !self.hovered && !self.dragging && !self.menu_open && !agent_needs_her {
            self.sleeping = true;
            self.next_z = self.now + 500;
        } else if self.sleeping && (idle < 1000 || self.hovered || agent_needs_her) {
            self.sleeping = false;
            self.happy_until = self.now + 900;
            let z = self.z_frames.clone();
            self.particles.retain(|p| !Rc::ptr_eq(&p.frames, &z));
        }
    }

    fn idle_action(&mut self) {
        // smile 35, twinkle 30, hop 15 (if she can), look up 30 (if she can)
        let roll = self.rng.below(65 + if self.can_bounce { 15 } else { 0 } + if self.has_look { 30 } else { 0 });
        if roll < 35 {
            self.smile_until = self.now + 1600;
        } else if roll < 65 {
            self.spawn_twinkle();
        } else if self.can_bounce && roll < 80 {
            self.bounce_at = self.now;
        } else {
            self.look_until = self.now + 1400 + self.rng.below(1200);
        }
    }

    /// The agent needs you: hop, or (a pet without a body animation) look up with a twinkle.
    fn call_for_attention(&mut self) {
        if self.can_bounce {
            self.bounce_at = self.now;
            return;
        }
        self.look_until = self.now + 1500;
        self.spawn_twinkle();
    }

    fn cheer(&mut self) {
        if self.can_bounce {
            self.bounce_at = self.now;
        } else {
            self.happy_until = self.now + 1400;
        }
    }

    fn face_name(&self) -> &'static str {
        let now = self.now;
        if self.dragging {
            return if self.has_drag { "drag" } else { "happy" };
        }
        if self.sleeping {
            return if self.has_sleep { "sleep" } else { "blink" };
        }
        if now < self.happy_until {
            return "happy";
        }
        let done = self.done_shown_since > 0 && self.agent == AgentState::Idle;
        if self.hovered || done || now < self.smile_until {
            return if self.has_hover {
                if now < self.blink_until { "blink" } else { "hover" }
            } else {
                "happy"
            };
        }
        if now < self.blink_until {
            return "blink";
        }
        if now < self.look_until && self.has_look {
            return "look";
        }
        "normal"
    }

    fn frame_name(&mut self) -> String {
        if self.bounce_at >= 0 {
            let mut t = self.now - self.bounce_at;
            if self.can_bounce {
                for i in 0..BOUNCE_STEPS.len() {
                    if t < BOUNCE_MS[i] {
                        return format!("bounce_{}", BOUNCE_STEPS[i]);
                    }
                    t -= BOUNCE_MS[i];
                }
            }
            self.bounce_at = -1;
        }
        let face = self.face_name();
        let name = format!("{face}_{}", self.phase);
        if self.atlas.has_frame(&name) { name } else { format!("{face}_0") }
    }

    /// Bubble sprite to show, or None. The prompt on hover wins over the agent's state.
    fn current_bubble(&self) -> Option<String> {
        if self.dragging || self.sleeping {
            return None;
        }
        let now = self.now;
        let bubble = if self.looks_left() { "bubble_l_" } else { "bubble_r_" };
        if (self.hovered && now - self.hover_since >= BUBBLE_DELAY_MS) || now < self.hint_until {
            return Some(format!("{bubble}{}", if (now / 530) % 2 == 0 { "on" } else { "off" }));
        }
        match self.agent {
            AgentState::Waiting => Some(format!("{bubble}wait")),
            AgentState::Working => {
                let frames = &self.spin_frames;
                Some(format!("{bubble}{}", frames[((now / 110) % frames.len() as i64) as usize]))
            }
            _ if self.done_shown_since > 0 => Some(format!("{bubble}done")),
            _ => None,
        }
    }

    /// Bubble bounds in canvas pixels.
    fn bubble_rect(&self) -> Rect {
        let tip = self.atlas.anchor("bubble", self.mirrored);
        let (w, h) = self.atlas.sprite_size("bubble_r_on");
        let left = if self.looks_left() { PAD_X + tip.x - (w - 1) } else { PAD_X + tip.x };
        Rect::new(left, PAD_TOP + tip.y - (h - 1), w, h)
    }

    fn render(&mut self, force: bool) {
        if self.surface.is_none() {
            return;
        }
        let frame = format!("{}{}", if self.mirrored { "m_" } else { "" }, self.frame_name());
        let bubble = self.current_bubble();
        let key = if self.hidden_for_fullscreen {
            "hidden".to_string()
        } else {
            format!("{frame}{}{}", bubble.as_deref().unwrap_or(""), self.particles.len())
        };
        if !force && self.last_render.as_deref() == Some(key.as_str()) && (self.particles.is_empty() || self.hidden_for_fullscreen) {
            return;
        }
        self.last_render = Some(key);
        let win = self.window_rect();
        let Some(mut surface) = self.surface.take() else { return };
        {
            let mut c = surface.canvas();
            c.clear(0);
            if !self.hidden_for_fullscreen {
                self.compose(&mut c, &frame, bubble.as_deref());
            }
        }
        surface.present(self.hwnd, win.x, win.y);
        self.surface = Some(surface);
    }

    fn compose(&self, c: &mut Canvas, frame: &str, bubble: Option<&str>) {
        self.atlas.draw_frame(c, frame, self.cell_px());
        if let Some(bubble) = bubble {
            let tip = self.atlas.anchor("bubble", self.mirrored);
            self.atlas.draw_sprite(c, bubble, self.px((PAD_X + tip.x) as f64), self.px((PAD_TOP + tip.y) as f64), self.zoom);
        }
        for p in &self.particles {
            let t = (self.now - p.born) as f64 / 1000.0;
            let f = (self.now - p.born) as f64 / p.life as f64;
            let index = ((f * p.frames.len() as f64) as usize).min(p.frames.len() - 1);
            let x = round(p.x + p.vx * t);
            let y = round(p.y + p.vy * t + 0.5 * p.gravity * t * t);
            self.atlas.draw_sprite(c, &p.frames[index], self.px(x as f64), self.px(y as f64), self.zoom);
        }
    }

    // ------------------------------------------------------------------ particles

    fn canvas_anchor(&self, name: &str) -> Pt {
        let a = self.atlas.anchor(name, self.mirrored);
        Pt::new(PAD_X + a.x, PAD_TOP + a.y)
    }

    fn burst(&mut self) {
        let head = self.canvas_anchor("head");
        let count = 8;
        let offset = self.rng.double() * std::f64::consts::PI;
        for i in 0..count {
            let angle = offset + i as f64 * 2.0 * std::f64::consts::PI / count as f64;
            let speed = 38.0 + self.rng.below(22) as f64;
            let life = 620 + self.rng.below(160);
            self.particles.push(Particle {
                x: head.x as f64,
                y: (head.y - 16) as f64, // above the hair, where the sparks stand out
                vx: angle.cos() * speed,
                vy: angle.sin() * speed - 22.0,
                gravity: 40.0,
                born: self.now,
                life,
                frames: self.burst_frames.clone(),
            });
        }
    }

    fn spawn_twinkle(&mut self) {
        let logo = self.canvas_anchor("logo");
        let vx = (self.rng.double() - 0.5) * 6.0;
        let frames = self.twinkles[self.rng.below(self.twinkles.len() as i64) as usize].clone();
        self.particles.push(Particle { x: logo.x as f64, y: logo.y as f64, vx, vy: -16.0, gravity: 0.0, born: self.now, life: 1100, frames });
    }

    fn spawn_z(&mut self) {
        let z = self.canvas_anchor("zzz");
        let vx = if self.looks_left() { -4.0 } else { 4.0 };
        self.particles.push(Particle {
            x: z.x as f64,
            y: z.y as f64,
            vx,
            vy: -7.0,
            gravity: 0.0,
            born: self.now,
            life: 2600,
            frames: self.z_frames.clone(),
        });
    }

    // ------------------------------------------------------------------ input

    fn mouse_down(&mut self) {
        self.pressed = true;
        self.dragging = false;
        self.press_cursor = win::cursor_pos();
        self.press_anchor = self.anchor;
    }

    fn mouse_move(&mut self) {
        if !self.pressed {
            return;
        }
        let c = win::cursor_pos();
        let (slop_w, slop_h) = win::drag_size();
        if !self.dragging && ((c.x - self.press_cursor.x).abs() > slop_w || (c.y - self.press_cursor.y).abs() > slop_h) {
            self.start_drag();
        }
        if self.dragging {
            let p = Pt::new(self.press_anchor.x + c.x - self.press_cursor.x, self.press_anchor.y + c.y - self.press_cursor.y);
            self.move_to(p, false);
        }
    }

    fn start_drag(&mut self) {
        self.dragging = true;
        self.sleeping = false;
        self.take_front_layer();
    }

    /// The pet you drag comes to the front and stays there, like a window in Windows. The new order is
    /// saved ([app] layers), and every other pet takes her place behind her at once.
    fn take_front_layer(&mut self) {
        if self.rank() != 0 {
            let (id, all) = (self.info.id.clone(), self.all_pets.clone());
            store::change(|ini| layers::bring_to_front(ini, &id, &all));
            self.layer_order.retain(|x| *x != id);
            self.layer_order.insert(0, id.clone());
            for other in &self.all_pets {
                if other.id != id {
                    ipc::post_to_pet(&other.id, ipc::CMD_RELOAD);
                }
            }
        }
        self.restack();
    }

    fn left_up(&mut self) {
        if !self.pressed {
            return;
        }
        self.pressed = false;
        if self.dragging {
            self.end_drag();
        } else {
            self.open_agent();
        }
    }

    fn capture_lost(&mut self) {
        if self.pressed && self.dragging {
            self.pressed = false;
            self.end_drag(); // capture lost mid-drag (e.g. Alt+Tab): drop her where she is
        }
    }

    fn end_drag(&mut self) {
        self.dragging = false;
        self.move_to(self.anchor, true);
        let before = self.height;
        self.fit_height(); // on another screen she may have to be smaller, or may grow back
        if self.height != before {
            self.move_to(self.anchor, true);
        }
        self.save_position();
        self.restack(); // she stays in front where she was dropped
    }

    fn open_agent(&mut self) {
        self.now = self.clock.elapsed().as_millis() as i64;
        self.sleeping = false;
        self.acknowledge_done();
        if self.can_bounce {
            self.bounce_at = self.now;
        }
        self.happy_until = self.now + 1400;
        self.burst();
        self.render(true);
        if self.now - self.last_launch < 1500 {
            return; // double clicks open one window, not two
        }
        self.last_launch = self.now;
        let s = self.read_settings();
        let info = self.info.clone();
        let dry_run = self.dry_run;
        std::thread::spawn(move || {
            if let Err(e) = launcher::launch(&info, &s, dry_run) {
                app::log_as(&info.id, &format!("launch failed: {e}"));
                tray::post(HostMsg::Alert { title: info.name.clone(), text: e, warn: true });
            }
        });
    }

    /// Only the position: heights are written where they change.
    fn save_position(&mut self) {
        store::update(&self.info.id, &[("x", Some(&app::number(self.anchor.x))), ("y", Some(&app::number(self.anchor.y)))]);
        self.settings.has_position = true;
        self.settings.x = self.anchor.x;
        self.settings.y = self.anchor.y;
        // do not acknowledge unrelated changes that were saved before this position write
    }

    fn read_settings(&self) -> PetSettings {
        store::try_load().map(|ini| PetSettings::from(&self.info, &ini)).unwrap_or_else(|| self.settings.clone())
    }

    // ------------------------------------------------------------------ menu

    fn build_menu(&mut self) -> HMENU {
        let s = self.read_settings();
        let id = &self.info;
        let m = menu::new();
        menu::item(m, M_OPEN, &id.open_text, false, true);
        menu::bold(m, M_OPEN);
        // only the terminal and an app opened by the program (codex app) use the working folder
        if s.opens_program() || (s.opens_app() && launcher::app_via_program(id, &s)) {
            menu::item(m, M_FOLDER, &format!("Folder: {} …", app::short_path(&s.work_dir)), false, true);
        }
        let opens = menu::new();
        menu::item(opens, M_WEBSITE, "Website", s.opens_website(), true);
        menu::item(opens, M_APP, &app_menu_text(id, &s), s.opens_app(), true);
        menu::item(opens, M_PROGRAM, "Program", s.opens_program(), true);
        menu::submenu(m, "Click opens", opens);
        menu::separator(m);
        // heights of all pets, then this pet's own, each group under a grey heading
        let sizes = menu::new();
        menu::item(sizes, 0, "All pets", false, false);
        for i in 1..=4 {
            let px = i * HEIGHT_UNIT;
            menu::item(sizes, M_SHARED + i as u32, &pets::height_text(px), px == s.shared_height(), true);
        }
        menu::separator(sizes);
        menu::item(sizes, 0, &format!("Only {}", id.name), false, false);
        let screen = self.screen_height();
        menu::item(sizes, M_LIKE_ALL, "Like all", s.own_height == 0, true);
        menu::item(sizes, M_SMALLER, "Smaller", false, self.height > MIN_HEIGHT);
        menu::item(sizes, M_BIGGER, "Bigger", false, self.height < screen);
        menu::item(sizes, M_SCREEN, "As tall as the screen", s.own_height > 0 && self.height == screen, true);
        menu::submenu(m, "Size", sizes);
        menu::item(m, M_HOME, "Back to the corner", false, true);
        menu::item(m, M_HIDE, "Hide", false, true);
        menu::separator(m);
        menu::item(m, M_SETTINGS, "Settings …", false, true);
        menu::item(m, M_QUIT, "Quit aipets", false, true);
        m
    }

    fn menu_command(&mut self, cmd: u32) {
        match cmd {
            M_OPEN => self.open_agent(),
            M_WEBSITE => self.set_mode("website"),
            M_APP => self.set_mode("app"),
            M_PROGRAM => self.set_mode("program"),
            c if c > M_SHARED && c <= M_SHARED + 4 => self.set_shared_height((c - M_SHARED) as i32 * HEIGHT_UNIT),
            M_LIKE_ALL => self.set_own_height(0),
            M_SMALLER => self.set_own_height(self.height * 4 / 5),
            M_BIGGER => self.set_own_height(self.height * 5 / 4),
            M_SCREEN => self.set_own_height(self.screen_height()),
            M_HOME => {
                self.move_to(self.home_anchor(), true);
                self.save_position();
            }
            M_HIDE => {
                // saved here as well: she stays hidden after a restart even if the tray does not answer now
                store::update(&self.info.id, &[("enabled", Some("0"))]);
                tray::post(HostMsg::Command(format!("hide {}", self.info.id)));
            }
            M_SETTINGS => tray::post(HostMsg::Command(format!("settings {}", self.info.id))),
            M_QUIT => tray::post(HostMsg::Command("quit".into())),
            _ => {}
        }
    }

    /// The height of all pets from the menu, like the first slider in the settings: every pet snaps to it.
    fn set_shared_height(&mut self, px: i32) {
        store::change(|ini| pets::share_height(ini, px));
        self.reload_settings(); // apply it now instead of waiting for the file stamp
        for other in PetInfo::discover() {
            if other.id != self.info.id {
                ipc::post_to_pet(&other.id, ipc::CMD_RELOAD);
            }
        }
    }

    /// This pet's own height (0: the one of all pets again), like the second slider.
    fn set_own_height(&mut self, px: i32) {
        let px = if px <= 0 { 0 } else { MIN_HEIGHT.max(self.screen_height().min(px)) };
        let value = app::number(px);
        store::update(&self.info.id, &[("height", if px == 0 { None } else { Some(&value) })]);
        self.reload_settings();
    }

    /// "program", "app" or "website"; the pet.ini default is stored as no override.
    fn set_mode(&mut self, mode: &str) {
        store::update(&self.info.id, &[("mode", if mode == self.info.mode { None } else { Some(mode) })]);
    }

    fn on_destroy(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER) };
        if !self.crashed {
            self.save_position();
        }
        self.log("closed");
    }

    // ------------------------------------------------------------------ snapshots

    fn reset_state(&mut self) {
        self.now = 21200; // bubble cursor phase: visible
        self.hovered = false;
        self.sleeping = false;
        self.bounce_at = -1;
        self.happy_until = 0;
        self.smile_until = 0;
        self.look_until = 0;
        self.blink_until = 0;
        self.phase = 0;
        self.agent = AgentState::Idle;
        self.done_shown_since = 0;
        self.particles.clear();
    }

    fn snapshot_state(&mut self, path: &str, setup: impl FnOnce(&mut Pet)) -> Result<(), String> {
        self.reset_state();
        setup(self);
        let (w, h) = (self.canvas_px_w(), self.canvas_px_h());
        let mut px: Vec<Pixel> = vec![atlas::rgb(58, 74, 92); (w * h) as usize];
        let frame = format!("{}{}", if self.mirrored { "m_" } else { "" }, self.frame_name());
        let bubble = self.current_bubble();
        self.compose(&mut Canvas::new(w, h, &mut px), &frame, bubble.as_deref());
        atlas::write_png(&format!("{path}.png"), w, h, &px)
    }
}

/// "Desktop app" in the menu, with which app a click opens and how (a tooltip in the old version).
fn app_menu_text(pet: &PetInfo, s: &PetSettings) -> String {
    if s.desktop_app.is_empty() {
        return "Desktop app".into();
    }
    let app = DesktopApp::find(&s.desktop_app);
    let text = app.as_ref().map(|a| a.to_string()).unwrap_or_else(|| "not found".into());
    let detail = if launcher::app_via_program(pet, s) {
        format!("{text}, via \"{}\"", launcher::program_command(s, &pet.app_command))
    } else if app.is_none() && !pet.app_fallback.is_empty() {
        format!("{text}, a click runs \"{}\"", launcher::program_command(s, &pet.app_fallback))
    } else {
        text
    };
    format!("Desktop app ({detail})")
}

/// A height that fits the work area of the screen at p: a pet can grow up to the full screen height.
pub fn fit_screen(p: Pt, px: i32) -> i32 {
    1.max(px.min(win::work_area_at(p).h))
}

/// Math.Round(zoom, 3) as .NET prints it: "3.375", "6".
fn three_decimals(v: f64) -> String {
    let s = format!("{:.3}", (v * 1000.0).round() / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl Window for Pet {
    fn message(slot: &Slot<Self>, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                slot.with(|p| p.on_created(hwnd))?;
                Some(0)
            }
            WM_MOUSEACTIVATE => Some(MA_NOACTIVATE as LRESULT), // clicking the pet never steals focus
            WM_TIMER if wp == TIMER => {
                slot.with(|p| p.tick());
                Some(0)
            }
            WM_DISPLAYCHANGE => {
                slot.with(|p| p.on_display_changed());
                None
            }
            WM_LBUTTONDOWN => {
                slot.with(|p| p.mouse_down());
                unsafe { SetCapture(hwnd) };
                Some(0)
            }
            WM_MOUSEMOVE => {
                slot.with(|p| p.mouse_move());
                Some(0)
            }
            WM_LBUTTONUP => {
                slot.with(|p| p.left_up());
                unsafe { ReleaseCapture() };
                Some(0)
            }
            WM_CAPTURECHANGED => {
                if lp as HWND != hwnd {
                    slot.with(|p| p.capture_lost());
                }
                Some(0)
            }
            WM_RBUTTONUP => {
                show_menu(slot, hwnd);
                Some(0)
            }
            WM_WINDOWPOSCHANGED => {
                // Windows moved her in the z-order (e.g. to the top with a menu or dialog): back into her layer at once.
                // Her own moves arrive while her state is busy and do not come here.
                let pos = unsafe { &*(lp as *const WINDOWPOS) };
                if pos.flags & SWP_NOZORDER == 0 {
                    slot.with(|p| {
                        if !p.hidden_for_fullscreen {
                            p.restack();
                        }
                    });
                }
                None
            }
            WM_DESTROY => {
                slot.with(|p| p.on_destroy());
                None
            }
            m if m == ipc::command_message() => {
                match wp {
                    ipc::CMD_RELOAD => slot.with(|p| p.reload_settings()),
                    ipc::CMD_LAUNCH => slot.with(|p| p.open_agent()),
                    _ => None,
                };
                Some(0)
            }
            _ => None,
        }
    }

    /// A broken pet goes away; the tray starts a fresh one.
    fn panicked(slot: &Slot<Self>, hwnd: HWND) {
        let id = slot.with(|p| {
            p.crashed = true;
            p.info.id.clone()
        });
        if let Some(id) = id {
            tray::post(HostMsg::Crashed(id));
        }
        unsafe { DestroyWindow(hwnd) };
    }
}

fn show_menu(slot: &Slot<Pet>, hwnd: HWND) {
    let Some(m) = slot.with(|p| {
        p.menu_open = true;
        p.build_menu()
    }) else {
        return;
    };
    let owner = tray::host_hwnd();
    let cmd = menu::track(m, if owner.is_null() { hwnd } else { owner }, &[M_FOLDER, M_SETTINGS]);
    slot.with(|p| p.menu_open = false);
    if cmd == M_FOLDER {
        // a dialog: without holding her state, so she keeps animating behind it
        let Some((id, name, dir)) = slot.with(|p| (p.info.id.clone(), p.info.name.clone(), p.read_settings().work_dir)) else { return };
        if let Some(chosen) = dialogs::browse_folder(hwnd, &format!("Which folder should {name} start in?"), &dir) {
            if win::dir_exists(&chosen) {
                store::update(&id, &[("workdir", Some(&chosen))]);
            }
        }
        return;
    }
    slot.with(|p| p.menu_command(cmd));
}

// ------------------------------------------------------------------ snapshots (--snapshot)

/// Art check without touching the desktop: renders idle, hover, look, sleep, click, working, waiting and
/// done (both looking directions) through the real drawing code into PNGs.
pub fn snapshot(info: &PetInfo, dir: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut pet = Pet::new(info.clone(), true)?;
    pet.zoom = 3.0; // whole cell pixels, for checking the art
    for mirror in [false, true] {
        pet.mirrored = mirror;
        let side = if pet.looks_left() { "_left" } else { "_right" };
        let prefix = win::combine(dir, &format!("{}_", info.id));
        pet.snapshot_state(&format!("{prefix}idle{side}"), |_| {})?;
        pet.snapshot_state(&format!("{prefix}hover{side}"), |p| {
            p.hovered = true;
            p.hover_since = p.now - 1000;
        })?;
        pet.snapshot_state(&format!("{prefix}look{side}"), |p| p.look_until = p.now + 1000)?;
        pet.snapshot_state(&format!("{prefix}sleep{side}"), |p| {
            p.sleeping = true;
            for _ in 0..3 {
                p.spawn_z();
                p.now += 800;
            }
        })?;
        pet.snapshot_state(&format!("{prefix}click{side}"), |p| {
            p.bounce_at = p.now;
            p.happy_until = p.now + 1400;
            p.burst();
            p.now += 260;
        })?;
        pet.snapshot_state(&format!("{prefix}working{side}"), |p| {
            p.agent = AgentState::Working;
            p.now = 110 * 196 - 360;
            p.spawn_twinkle();
            p.now = 110 * 196; // biggest frame of the pulse spinner
        })?;
        pet.snapshot_state(&format!("{prefix}waiting{side}"), |p| p.agent = AgentState::Waiting)?;
        pet.snapshot_state(&format!("{prefix}done{side}"), |p| p.done_shown_since = 1)?;
    }
    Ok(())
}

/// Size check: every pet idle at 324 px (without her own height), side by side on one baseline.
/// All heads should touch the line 324 px above the ground line.
pub fn snapshot_lineup(infos: &[PetInfo], path: &str) -> Result<(), String> {
    let mut pets = Vec::new();
    let (mut width, mut height) = (0, 0);
    for info in infos.iter().filter(|i| i.has_sprites()) {
        let mut pet = Pet::new(info.clone(), true)?;
        pet.set_height(2 * HEIGHT_UNIT);
        pet.mirrored = false;
        width += pet.canvas_px_w();
        height = height.max(pet.canvas_px_h());
        pets.push(pet);
    }
    let (w, h) = (width.max(1), height + 2);
    let mut px: Vec<Pixel> = vec![atlas::rgb(58, 74, 92); (w * h) as usize];
    {
        let mut c = Canvas::new(w, h, &mut px);
        let mut x = 0;
        for pet in &mut pets {
            pet.reset_state();
            c.origin = Pt::new(x, height - pet.canvas_px_h());
            let frame = pet.frame_name();
            pet.compose(&mut c, &frame, None);
            x += pet.canvas_px_w();
        }
        c.origin = Pt::default();
        let line = atlas::rgb(255, 110, 110);
        c.fill_rect(Rect::new(0, height, w, 1), line);
        c.fill_rect(Rect::new(0, height - 2 * HEIGHT_UNIT - 1, w, 1), line);
    }
    atlas::write_png(path, w, h, &px)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_text_like_dotnet() {
        assert_eq!(three_decimals(3.0), "3");
        assert_eq!(three_decimals(3.3750001), "3.375");
        assert_eq!(three_decimals(2.5), "2.5");
    }

    #[test]
    fn rng_ranges() {
        let mut r = Rng::new();
        for _ in 0..1000 {
            let v = r.range(2200, 5600);
            assert!((2200..5600).contains(&v));
            let d = r.double();
            assert!((0.0..1.0).contains(&d));
        }
    }
}
