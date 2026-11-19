//! Fixed layers. Overlapping pets stack in a fixed order: the pet dragged last stands in front, like a
//! window in Windows, and without dragging the menu order holds. Menus, dialogs and tooltips of aipets
//! stay above every pet. Each pet moves only herself, directly below the lowest window she must not
//! cover, and only when she is not there yet.

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::Ini;
use crate::ipc;
use crate::pets::PetInfo;

pub const CYCLE_MS: i64 = 3000;
pub const STEP_MS: i64 = 200;
pub const RETRY_MS: i64 = 100;
const MAX_WINDOWS: usize = 10000; // z-order walks end here even if windows reshuffle meanwhile

/// Pet ids from the front to the back. [app] layers keeps the order the user made by dragging;
/// pets missing there follow in menu order. Without the key it is the menu order.
pub fn order(settings: &Ini, pets: &[PetInfo]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for part in settings.get("app", "layers").unwrap_or("").split(',') {
        let id = part.trim().to_lowercase();
        if !id.is_empty() && !order.contains(&id) {
            order.push(id); // ids of pets this copy does not know keep their place for the others
        }
    }
    for pet in pets {
        if !order.contains(&pet.id) {
            order.push(pet.id.clone());
        }
    }
    order
}

/// A pet that is dragged comes to the front and stays there: saves the new order.
pub fn bring_to_front(settings: &mut Ini, id: &str, pets: &[PetInfo]) {
    let mut o = order(settings, pets);
    o.retain(|x| x != id);
    o.insert(0, id.to_string());
    settings.set("app", "layers", Some(&o.join(",")));
}

/// A visible window of the topmost band.
#[derive(Clone, Debug)]
pub struct Window {
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
}

/// The window a pet belongs directly below, or null for the very top.
/// band: the visible topmost windows from the top down. order: pet ids, the front one first.
/// processes: aipets' own; every process with a pet window counts as well.
/// Pets missing from the order never count as in front of her.
pub fn below(band: &[Window], me: HWND, my_id: &str, order: &[String], processes: &[u32]) -> HWND {
    let mut ours: Vec<u32> = processes.to_vec();
    for w in band {
        if pet_id(&w.title).is_some() && !ours.contains(&w.pid) {
            ours.push(w.pid);
        }
    }
    let rank = order.iter().position(|x| x == my_id).map(|i| i as i64).unwrap_or(-1);
    let mut result: HWND = std::ptr::null_mut();
    for w in band {
        if w.hwnd == me {
            continue;
        }
        let stays = match pet_id(&w.title) {
            None => ours.contains(&w.pid), // a menu, dialog or tooltip of aipets
            Some(id) => matches!(order.iter().position(|x| x == id), Some(i) if (i as i64) < rank), // a pet in front of her
        };
        if stays {
            result = w.hwnd; // the band runs from the top down: the last one is the lowest
        }
    }
    result
}

/// "claude" for the window title "aipets.pet.claude", None for any other title.
pub fn pet_id(title: &str) -> Option<&str> {
    let prefix = ipc::PET_TITLE_PREFIX;
    if title.len() > prefix.len() && title.starts_with(prefix) {
        Some(&title[prefix.len()..])
    } else {
        None
    }
}

/// When a pet checks her layer next (unix ms). All pets share one 3 s grid, the front pet first and
/// every further pet 200 ms later: front to back, a single cycle puts all of them in place.
pub fn next_check(now_ms: i64, rank: i64) -> i64 {
    let offset = rank.max(0) * STEP_MS;
    ((now_ms - offset).div_euclid(CYCLE_MS) + 1) * CYCLE_MS + offset
}

/// Puts the pet into her layer. Looking again after a move catches a menu that opened in between.
/// False if windows kept moving and she could not check her place: try again soon.
pub fn restack(me: HWND, my_id: &str, order: &[String], processes: &[u32]) -> bool {
    let mut moves = 0;
    for _ in 0..5 {
        if moves >= 2 {
            break;
        }
        let Some(band) = topmost_windows() else { continue }; // windows moved during the walk: look again
        let target = below(&band, me, my_id, order, processes);
        if is_topmost(me) && visible_window_above(me) == target {
            return true;
        }
        place_below(me, target);
        moves += 1;
    }
    moves > 0
}

/// The shown topmost windows, from the top of the z-order down, or None while windows move.
/// A walk is no snapshot: a window that moves meanwhile can be missed or seen twice, so two walks must agree.
fn topmost_windows() -> Option<Vec<Window>> {
    let first = walk_topmost()?;
    let second = walk_topmost()?;
    if first.len() != second.len() || first.iter().zip(&second).any(|(a, b)| a.hwnd != b.hwnd) {
        return None;
    }
    Some(first)
}

fn walk_topmost() -> Option<Vec<Window>> {
    let mut band = Vec::new();
    unsafe {
        let mut hwnd = GetTopWindow(std::ptr::null_mut());
        let mut last: HWND = std::ptr::null_mut();
        let mut i = 0;
        while !hwnd.is_null() && i < MAX_WINDOWS {
            // topmost windows can also sit further down, hidden ones even below normal windows: walk everything
            if is_topmost(hwnd) && shown(hwnd) {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, &mut pid);
                let mut title = [0u16; 256];
                let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
                band.push(Window { hwnd, pid, title: String::from_utf16_lossy(&title[..n.max(0) as usize]) });
            }
            last = hwnd;
            hwnd = GetWindow(hwnd, GW_HWNDNEXT);
            i += 1;
        }
        let complete = hwnd.is_null() && !last.is_null() && GetWindow(last, GW_HWNDLAST) == last;
        if complete { Some(band) } else { None }
    }
}

pub fn is_topmost(hwnd: HWND) -> bool {
    unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0 }
}

/// Visible and at least 2×2 px: helper windows cover nothing.
fn shown(hwnd: HWND) -> bool {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        IsWindowVisible(hwnd) != 0 && GetWindowRect(hwnd, &mut r) != 0 && r.right - r.left > 1 && r.bottom - r.top > 1
    }
}

/// The nearest shown window above this one, null at the very top, or HWND_TOPMOST if the walk broke off.
fn visible_window_above(mut hwnd: HWND) -> HWND {
    unsafe {
        for _ in 0..MAX_WINDOWS {
            let above = GetWindow(hwnd, GW_HWNDPREV);
            if above.is_null() {
                return if GetWindow(hwnd, GW_HWNDFIRST) == hwnd { std::ptr::null_mut() } else { HWND_TOPMOST };
            }
            if shown(above) {
                return above;
            }
            hwnd = above;
        }
        HWND_TOPMOST
    }
}

/// Moves a topmost window directly below another topmost window, or with null to the very top.
/// Only this window moves (no owner or owned windows), and it is not activated.
fn place_below(hwnd: HWND, above: HWND) {
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    unsafe {
        if above.is_null() || !is_topmost(hwnd) {
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
        }
        if !above.is_null() {
            SetWindowPos(hwnd, above, 0, 0, 0, 0, flags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(h: isize, pid: u32, title: &str) -> Window {
        Window { hwnd: h as HWND, pid, title: title.into() }
    }

    #[test]
    fn below_respects_order_and_own_popups() {
        let order: Vec<String> = ["claude", "hermes", "astra"].iter().map(|s| s.to_string()).collect();
        let band = vec![
            w(1, 50, "menu"),            // own menu
            w(2, 99, "foreign topmost"), // someone else's
            w(3, 50, "aipets.pet.claude"),
            w(4, 50, "aipets.pet.hermes"),
            w(5, 50, "aipets.pet.astra"),
        ];
        assert_eq!(below(&band, 5 as HWND, "astra", &order, &[50]), 4 as HWND);
        assert_eq!(below(&band, 3 as HWND, "claude", &order, &[50]), 1 as HWND);
        assert_eq!(below(&band, 3 as HWND, "unknown", &order, &[50]), 1 as HWND);
        let foreign = vec![w(2, 99, "x"), w(3, 50, "aipets.pet.claude")];
        assert!(below(&foreign, 3 as HWND, "claude", &order, &[50]).is_null());
    }

    #[test]
    fn grid_and_order() {
        assert_eq!(next_check(0, 0), 3000);
        assert_eq!(next_check(2999, 0), 3000);
        assert_eq!(next_check(3000, 1), 3200);
        assert_eq!(next_check(3300, 1), 6200);
        let ini = Ini::parse("[app]\nlayers=Astra, gemini,astra,,ghost\n", "app");
        let pets: Vec<PetInfo> = ["claude", "astra"].iter().map(|id| PetInfo { id: id.to_string(), ..Default::default() }).collect();
        assert_eq!(order(&ini, &pets), vec!["astra", "gemini", "ghost", "claude"]);
        let mut ini = ini;
        bring_to_front(&mut ini, "claude", &pets);
        assert_eq!(ini.get("app", "layers"), Some("claude,astra,gemini,ghost"));
    }
}
