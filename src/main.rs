//! aipets.exe                          tray: starts and watches the pets (all in this one process)
//! aipets.exe --hook <source> <event>  agent hook: record the state and exit
//! aipets.exe --hook cursor            Cursor hook, the event comes with the payload
//! aipets.exe (started by Cursor)      also a Cursor hook: CURSOR_VERSION is set and the payload is piped in
//! aipets.exe --snapshot <dir> [--pet id]  render pets, all pets side by side and the settings window
//!                                     (that pet's page, once per mode) into PNGs
//! aipets.exe --status <source>        print the folded agent state (diagnostics)
//! aipets.exe --command <id> [--mode m] print what a click on the pet would start (diagnostics; m = program, app, website)
//! aipets.exe --install [--quiet]      autostart + status hooks of the installed agents, then start the tray
//! aipets.exe --uninstall [--quiet]    quit the tray, remove autostart and hooks
//! --dry-run: clicks only log what they would start

#![windows_subsystem = "windows"]

mod app;
mod atlas;
mod dialogs;
mod ipc;
mod json;
mod launcher;
mod layers;
mod pet;
mod pets;
mod settings;
mod setup;
mod status;
mod tray;
mod win;
mod win_com;
mod wnd;

use std::process::Command;

use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO};

use crate::app::store;
use crate::pets::{PetInfo, PetSettings};
use crate::setup::Step;
use crate::win::NamedMutex;

fn main() {
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    // agent hook: first, so this path exits within milliseconds
    if args.first().map(String::as_str) == Some("--hook") && (args.len() >= 3 || (args.len() == 2 && args[1] == "cursor")) {
        status::hook_run(&args[1], args.get(2).map(String::as_str));
        return 0;
    }
    // Cursor runs its hooks and Claude Code's without arguments: never start the tray (or open its settings) for them
    if args.is_empty() && status::run_cursor_hook() {
        return 0;
    }
    app::install_panic_hook();
    let has = |name: &str| args.iter().any(|a| a == name);
    let dry_run = has("--dry-run");
    let pet_id = option(args, "--pet");

    if let Some(source) = option(args, "--status") {
        win::attach_console();
        let mut monitor = status::StatusMonitor::new(source, false);
        monitor.scan();
        let done = if monitor.latest_done > 0 { format!(" (last done {})", monitor.latest_done) } else { String::new() };
        println!("{}{done}", monitor.state.name());
        return 0;
    }
    if let Some(id) = option(args, "--command") {
        win::attach_console();
        return print_command(id, option(args, "--mode"));
    }
    let install = has("--install");
    if install || has("--uninstall") {
        win::attach_console();
        return run_setup(install, has("--quiet"));
    }
    if let Some(dir) = option(args, "--snapshot") {
        app::set_log_tag("snapshot");
        return snapshot(dir, pet_id);
    }
    if pet_id.is_some() {
        // pets were separate processes before 0.2; an old tray or shortcut may still start one
        app::set_log_tag("tray");
        app::log("--pet is no longer used: the tray shows every pet itself");
        return 2;
    }
    tray::run(dry_run)
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).map(String::as_str)
}

fn snapshot(dir: &str, pet_id: Option<&str>) -> i32 {
    let mut code = 0;
    let all = PetInfo::discover();
    for pet in all.iter().filter(|p| pet_id.map_or(true, |id| p.id == id)) {
        if let Err(e) = pet::snapshot(pet, dir) {
            app::log(&format!("snapshot {}: {e}", pet.id));
            code = 1;
        }
    }
    if let Err(e) = pet::snapshot_lineup(&all, &win::combine(dir, "lineup.png")) {
        app::log(&format!("snapshot lineup: {e}"));
        code = 1;
    }
    if let Err(e) = settings::snapshot(dir, pet_id) {
        app::log(&format!("snapshot settings: {e}"));
        code = 1;
    }
    code
}

/// --command: what a click would open, in the saved mode or the given one; errors are printed.
fn print_command(id: &str, mode: Option<&str>) -> i32 {
    let Some(info) = PetInfo::by_id(id) else {
        println!("No pet with the id {id}");
        return 2;
    };
    let mut s = PetSettings::from(&info, &store::load());
    if let Some(mode) = mode {
        s.use_mode(mode);
    }
    match launcher::describe(&info, &s) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            println!("{e}");
            1
        }
    }
}

/// install.cmd / uninstall.cmd: everything a user would otherwise set up by hand, with a summary at the end.
fn run_setup(install: bool, quiet: bool) -> i32 {
    app::set_log_tag(if install { "install" } else { "uninstall" });
    if !install
        && !quiet
        && win::message_box(
            std::ptr::null_mut(),
            "Remove aipets?\n\nThis quits aipets and removes \"Start with Windows\" as well as the status hooks from Claude Code, Codex, Hermes and Cursor.",
            app::NAME,
            MB_YESNO | MB_ICONQUESTION,
        ) != IDYES
    {
        return 1;
    }
    if !install {
        quit_tray();
    }
    let exe = app::exe_path();
    let mut steps = if install { setup::install(exe) } else { setup::uninstall(exe) };
    if install {
        if tray_running() {
            steps.push(Step::new("aipets", true, "running"));
        } else {
            // through explorer, so the tray does not inherit the installer's environment
            let _ = Command::new("explorer.exe").raw_arg(format!("\"{exe}\"")).spawn();
            steps.push(Step::new("aipets", true, "started (icon in the notification area, left click = settings)"));
        }
    }
    let ok = steps.iter().all(|s| s.ok || s.text == "not installed");
    let mut text = String::from(if install { "aipets is set up." } else { "aipets is removed." });
    text.push_str(if ok { "\n\n" } else { " Not everything worked, see below.\n\n" });
    for step in &steps {
        text.push_str(&format!("{step}\n"));
    }
    if !install {
        text.push_str(&format!("\nYou can now delete the folder {}.", app::dir()));
    }
    app::log(&text.replace('\n', " "));
    println!("{text}");
    if !quiet {
        win::message_box(std::ptr::null_mut(), &text, app::NAME, MB_OK | if ok { MB_ICONINFORMATION } else { MB_ICONWARNING });
    }
    if ok { 0 } else { 1 }
}

fn tray_running() -> bool {
    matches!(NamedMutex::create(r"Local\aipets.tray", false), Some((_, false)))
}

fn quit_tray() {
    if !tray_running() || !ipc::send_to_host("quit") {
        return;
    }
    for _ in 0..50 {
        if !tray_running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

trait RawArg {
    fn raw_arg(&mut self, arg: String) -> &mut Self;
}

impl RawArg for Command {
    fn raw_arg(&mut self, arg: String) -> &mut Self {
        std::os::windows::process::CommandExt::raw_arg(self, arg)
    }
}
