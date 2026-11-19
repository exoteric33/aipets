//! What a click opens: the pet's program in a new Windows Terminal window (plain console as fallback),
//! its desktop app, or its website in the default browser.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::app;
use crate::pets::{PetInfo, PetSettings};
use crate::win;

/// How a click starts something.
#[derive(Clone, Debug, PartialEq)]
pub enum StartInfo {
    /// Through the shell, like a double-click: a link or "shell:AppsFolder\<app id>".
    Shell { target: String },
    /// A process with this command line, in this folder, with this environment (None: aipets' own).
    Process { file: String, args: String, work_dir: String, env: Option<Vec<(String, String)>>, hidden: bool },
}

pub fn launch(pet: &PetInfo, s: &PetSettings, dry_run: bool) -> Result<(), String> {
    let start = build_start_info(pet, s)?;
    let line = match &start {
        StartInfo::Shell { target } => format!("{target} "),
        StartInfo::Process { file, args, env, .. } => {
            format!("{file} {args}{}", if env.is_none() && std::env::var_os("CLAUDECODE").is_some() { "  [inherited env!]" } else { "" })
        }
    };
    app::log(&format!("{}{line}", if dry_run { "dry-run: " } else { "launch: " }));
    if dry_run {
        return Ok(());
    }
    if s.opens_app() {
        win::allow_any_foreground(); // this process just got the click: let the app come to the front
    }
    match start {
        StartInfo::Shell { target } => shell_open(&target),
        StartInfo::Process { file, args, work_dir, env, hidden: false } => {
            command(&file, &args, &work_dir, env.as_deref()).spawn().map(|_| ()).map_err(|e| format!("{file}: {e}"))
        }
        StartInfo::Process { file, args, work_dir, env, hidden: true } => {
            // "codex app" has no window of its own: its messages go to the log, a failure to the user
            let (code, output) = run_hidden(command(&file, &args, &work_dir, env.as_deref()), Duration::from_secs(60))?;
            app::log(&format!(
                "app command {}{}",
                code.map(|c| format!("exit {c}")).unwrap_or_else(|| "still running after 60 s".into()),
                if output.is_empty() { String::new() } else { format!(": {}", output.replace("\r\n", " | ").replace('\n', " | ")) }
            ));
            match code {
                Some(c) if c != 0 => Err(format!(
                    "{}: \"{}\" failed with code {c}.{}",
                    pet.name,
                    program_command(s, &pet.app_command),
                    if output.is_empty() { String::new() } else { format!("\n\n{output}") }
                )),
                _ => Ok(()),
            }
        }
    }
}

fn command(file: &str, args: &str, work_dir: &str, env: Option<&[(String, String)]>) -> Command {
    let mut c = Command::new(file);
    if !args.is_empty() {
        c.raw_arg(args);
    }
    if !work_dir.is_empty() {
        c.current_dir(work_dir);
    }
    if let Some(env) = env {
        c.env_clear();
        for (k, v) in env {
            c.env(k, v);
        }
    }
    c
}

fn shell_open(target: &str) -> Result<(), String> {
    let _com = crate::win_com::ComInit::new();
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            win::wide("open").as_ptr(),
            win::wide(target).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if r > 32 { Ok(()) } else { Err(format!("Windows could not open \"{target}\" (error {r}).")) }
}

/// Runs a windowless command to its end, at most `limit`: its exit code (None if it still runs) and what
/// it printed. Output pipes that its children keep open are not waited for.
pub fn run_hidden(mut cmd: Command, limit: Duration) -> Result<(Option<i32>, String), String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let started = Instant::now();
    let mut code = None;
    let mut out = Vec::new();
    loop {
        while let Ok(chunk) = rx.try_recv() {
            out.extend(chunk);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                code = Some(status.code().unwrap_or(-1));
                break;
            }
            Ok(None) if started.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            _ => break,
        }
    }
    if code.is_some() {
        // the last lines, unless a child holds the pipes
        let until = Instant::now() + Duration::from_secs(1);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(chunk) => out.extend(chunk),
                Err(_) => break,
            }
        }
    }
    Ok((code, String::from_utf8_lossy(&out).trim().to_string()))
}

/// Program mode: see terminal_start_info. Website mode opens the link in the default browser.
/// App mode: a pet with appcommand lets its program open the app (codex app, in the working folder, no
/// window); otherwise the app starts directly. Without an app, a pet with appfallback runs its program
/// with those arguments in the terminal (hermes desktop builds Hermes Desktop once, then starts it).
pub fn build_start_info(pet: &PetInfo, s: &PetSettings) -> Result<StartInfo, String> {
    if s.opens_website() {
        return match normalize_url(&s.url) {
            Some(url) => Ok(StartInfo::Shell { target: url }),
            None => Err(format!(
                "{}: \"{}\" is not a link.\n\nEnter a link starting with http:// or https:// in the settings.",
                pet.name, s.url
            )),
        };
    }
    if s.opens_app() {
        let env = win::logon_environment();
        let program = if uses_app_command(pet, s) { program_path(pet, s, env.as_deref()) } else { None };
        if let Some(program) = program {
            return Ok(hidden_start_info(&program, &pet.app_command, &work_dir_of(s), env));
        }
        if let Some(app) = DesktopApp::find(&s.desktop_app) {
            return Ok(match &app.app_id {
                Some(id) => StartInfo::Shell { target: format!("shell:AppsFolder\\{id}") },
                None => {
                    let exe = app.exe.clone().unwrap_or_default();
                    StartInfo::Process { work_dir: win::dir_name(&exe), file: exe, args: String::new(), env, hidden: false }
                }
            });
        }
        if pet.app_fallback.is_empty() {
            return Err(format!(
                "{}: The desktop app was not found.\n\nInstall it or pick its exe under \"App\" in the settings.",
                pet.name
            ));
        }
        return terminal_start_info(pet, s, &pet.app_fallback);
    }
    terminal_start_info(pet, s, &s.args)
}

/// What a click would open, in words (--command).
pub fn describe(pet: &PetInfo, s: &PetSettings) -> Result<String, String> {
    let start = build_start_info(pet, s)?;
    let app = if s.opens_app() { DesktopApp::find(&s.desktop_app) } else { None };
    Ok(match start {
        StartInfo::Shell { target } if s.opens_website() => format!("{target}\n(in the default browser)"),
        StartInfo::Shell { target } => {
            let app = app.unwrap_or_default();
            format!("{target}\n(desktop app {app}, App package {})", app.family().unwrap_or_default())
        }
        StartInfo::Process { file, args, work_dir, hidden: true, .. } => format!(
            "{file} {args}\n(no window, in {work_dir}{})",
            match &app {
                Some(a) => format!("; opens the desktop app {a}"),
                None => "; desktop app not found".to_string(),
            }
        ),
        StartInfo::Process { file, args, work_dir, .. } => match &app {
            Some(a) => format!("{}\n(desktop app {a}, in {work_dir})", format!("{file} {args}").trim_end()),
            None => format!(
                "{file} {args}\n(in {work_dir}){}",
                if s.opens_app() {
                    format!("\n(desktop app not found, so \"{}\")", program_command(s, &pet.app_fallback))
                } else {
                    String::new()
                }
            ),
        },
    })
}

/// App mode goes through the program (appcommand): the pet has one, the app is not one the user picked,
/// and the program is installed. The working folder matters then.
pub fn app_via_program(pet: &PetInfo, s: &PetSettings) -> bool {
    uses_app_command(pet, s) && program_path(pet, s, win::logon_environment().as_deref()).is_some()
}

fn uses_app_command(pet: &PetInfo, s: &PetSettings) -> bool {
    !pet.app_command.is_empty() && s.desktop_app == pet.desktop_app
}

/// The pet's program, found with the PATH of this environment (None: aipets' own PATH).
fn program_path(pet: &PetInfo, s: &PetSettings, env: Option<&[(String, String)]>) -> Option<String> {
    resolve(&s.program, &pet.find, &path_of(env))
}

fn path_of(env: Option<&[(String, String)]>) -> String {
    env.and_then(|e| win::env_lookup(e, "PATH").map(str::to_string))
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default())
}

fn work_dir_of(s: &PetSettings) -> String {
    if win::dir_exists(&s.work_dir) { s.work_dir.clone() } else { win::user_profile().to_string_lossy().into_owned() }
}

/// A command without any window, run in the working folder: .cmd shims (npm installs) through cmd.exe.
fn hidden_start_info(program: &str, arguments: &str, work_dir: &str, env: Option<Vec<(String, String)>>) -> StartInfo {
    let args = arguments.trim();
    let (file, args) = if is_batch(program) {
        ("cmd.exe".to_string(), format!("/d /s /c \"{}{}\"", quote(program), if args.is_empty() { String::new() } else { format!(" {args}") }))
    } else {
        (program.to_string(), args.to_string())
    };
    StartInfo::Process { file, args, work_dir: work_dir.to_string(), env, hidden: true }
}

/// shell=direct: the terminal runs the program itself (tab closes when it exits);
/// shell=powershell / cmd: the program runs inside that shell, which stays open afterwards.
fn terminal_start_info(pet: &PetInfo, s: &PetSettings, arguments: &str) -> Result<StartInfo, String> {
    // wt hands the caller's environment to the new tab, so start from a clean logon environment
    // (also picks up PATH changes made while aipets runs)
    let env = win::logon_environment();
    let path = path_of(env.as_deref());
    let program = program_path(pet, s, env.as_deref()).ok_or_else(|| {
        format!("{}: \"{}\" was not found.\n\nEnter the full path in the settings or add its folder to PATH.", pet.name, s.program)
    })?;
    let work_dir = work_dir_of(s);
    let args = arguments.trim();
    let tail = if args.is_empty() { String::new() } else { format!(" {args}") };
    let command = match s.shell.as_str() {
        "powershell" => {
            let call = format!("& '{}'{tail}", program.replace('\'', "''"));
            format!("powershell.exe -NoLogo -NoExit -Command \"{}\"", call.replace('"', "\\\""))
        }
        "cmd" => format!("cmd.exe /k \"{}{tail}\"", quote(&program)),
        // .cmd shims (npm installs) need cmd.exe; a real exe runs directly
        _ => format!("{}{tail}", if is_batch(&program) { format!("cmd.exe /c {}", quote(&program)) } else { quote(&program) }),
    };
    let (file, args) = if let Some(terminal) = find_windows_terminal(&path) {
        (terminal, format!("-d {} {command}", quote(&terminal_dir(&work_dir))))
    } else if s.shell == "powershell" || s.shell == "cmd" {
        let sp = command.find(' ').unwrap_or(command.len());
        (command[..sp].to_string(), command[(sp + 1).min(command.len())..].to_string())
    } else {
        ("cmd.exe".to_string(), format!("/k \"{command}\""))
    };
    Ok(StartInfo::Process { file, args, work_dir, env, hidden: false })
}

/// Full path of the program: a path as given, else the pet's known install locations ("find" in pet.ini,
/// only if they are this program), else the PATH.
pub fn resolve(program: &str, find: &str, path: &str) -> Option<String> {
    let program = win::expand_env(program.trim().trim_matches('"'));
    if program.is_empty() {
        return None;
    }
    if program.contains('\\') || program.contains('/') {
        return if win::file_exists(&program) { Some(win::full_path(&program)) } else { None };
    }
    let bare = win::file_stem(&program).to_string();
    for hint in find.split(';') {
        let candidate = win::expand_env(hint.trim());
        if !candidate.is_empty() && win::file_exists(&candidate) && win::eq_ignore_case(win::file_stem(&candidate), &bare) {
            return Some(candidate);
        }
    }
    if win::has_extension(&program) {
        return find_on_path(path, &program);
    }
    [".exe", ".cmd", ".bat", ".com"].iter().find_map(|ext| find_on_path(path, &format!("{program}{ext}")))
}

/// An absolute http(s) link from what was typed ("gemini.google.com" gets https://), or None.
pub fn normalize_url(text: &str) -> Option<String> {
    let mut text = text.trim().to_string();
    if !text.is_empty() && !text.contains("://") {
        text = format!("https://{text}");
    }
    let sep = text.find("://")?;
    let scheme = text[..sep].to_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &text[sep + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let tail = &rest[end..];
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = if host_port.starts_with('[') {
        host_port.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\\' | '^' | '`' | '{' | '|' | '}')) {
        return None;
    }
    if let Some(port) = host_port.strip_prefix(&host).and_then(|p| p.strip_prefix(':')) {
        if !port.is_empty() && port.parse::<u16>().is_err() {
            return None;
        }
    }
    let mut authority = authority.to_lowercase();
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    if authority.ends_with(default_port) {
        authority.truncate(authority.len() - default_port.len());
    }
    let mut tail = tail.replace(' ', "%20");
    if !tail.starts_with('/') {
        tail = format!("/{tail}");
    }
    Some(format!("{scheme}://{authority}{tail}"))
}

fn is_batch(file: &str) -> bool {
    win::ends_with_ignore_case(file, ".cmd") || win::ends_with_ignore_case(file, ".bat")
}

fn find_windows_terminal(path: &str) -> Option<String> {
    let alias = win::local_app_data().join(r"Microsoft\WindowsApps\wt.exe").to_string_lossy().into_owned();
    if win::file_exists(&alias) { Some(alias) } else { find_on_path(path, "wt.exe") }
}

fn find_on_path(path: &str, file: &str) -> Option<String> {
    for dir in path.split(';') {
        let dir = dir.trim();
        if dir.is_empty() || dir.contains('"') {
            continue;
        }
        let candidate = win::combine(&win::expand_env(dir), file);
        if win::file_exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// wt parses its own command line: a trailing backslash would escape the closing quote.
fn terminal_dir(dir: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}.") } else { dir.to_string() }
}

fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// "codex app", "hermes desktop": the pet's program with these arguments, as one would type it.
pub fn program_command(s: &PetSettings, args: &str) -> String {
    let program = win::file_stem(s.program.trim().trim_matches('"')).to_string();
    format!("{}{args}", if program.is_empty() { String::new() } else { format!("{program} ") })
}

// ------------------------------------------------------------------ desktop apps

/// The desktop app a pet opens in app mode: an installed app package (Claude, the Codex app), started
/// through its app id like a Start menu click, or a plain exe (Hermes Desktop). pet.ini lists candidates
/// separated by ';', the first installed one wins.
#[derive(Clone, Debug, Default)]
pub struct DesktopApp {
    /// "PackageFamilyName!App" of a packaged app
    pub app_id: Option<String>,
    /// its full package name (name_version_arch__publisher)
    pub package: Option<String>,
    /// the exe of a plain app
    pub exe: Option<String>,
}

impl DesktopApp {
    pub fn find(candidates: &str) -> Option<DesktopApp> {
        let whole = expand(candidates);
        if whole.contains(';') && win::file_exists(&whole) {
            return Some(DesktopApp { exe: Some(win::full_path(&whole)), ..Default::default() }); // one exe whose path contains ';'
        }
        for part in candidates.split(';') {
            let candidate = expand(part);
            if candidate.is_empty() {
                continue;
            }
            if is_app_id(&candidate) {
                let family = &candidate[..candidate.find('!').unwrap()];
                if let Some(package) = installed_package(family) {
                    return Some(DesktopApp { app_id: Some(candidate.clone()), package: Some(package), exe: None });
                }
            } else if win::file_exists(&candidate) {
                return Some(DesktopApp { exe: Some(win::full_path(&candidate)), ..Default::default() });
            }
        }
        None
    }

    /// Package family ("Claude_pzs8sxrjxfjjc") of a packaged app.
    pub fn family(&self) -> Option<String> {
        self.app_id.as_ref().map(|id| id[..id.find('!').unwrap_or(id.len())].to_string())
    }

    /// The name Windows shows: the Start menu entry of a package, the file description of an exe.
    pub fn name(&self) -> String {
        let shown = match (&self.app_id, &self.exe) {
            (Some(id), _) => crate::win_com::shell_display_name(&format!("shell:AppsFolder\\{id}")),
            (None, Some(exe)) => exe_info(exe, true),
            _ => None,
        };
        if let Some(n) = shown.filter(|n| !n.is_empty()) {
            return n;
        }
        match (&self.package, &self.exe) {
            (Some(p), _) => p.split('_').next().unwrap_or("").to_string(),
            (None, Some(exe)) => win::file_stem(exe).to_string(),
            _ => String::new(),
        }
    }

    /// Package version or the exe's product version.
    pub fn version(&self) -> Option<String> {
        match (&self.package, &self.exe) {
            (Some(p), _) => p.split('_').nth(1).map(str::to_string),
            (None, Some(exe)) => exe_info(exe, false),
            _ => None,
        }
    }
}

impl std::fmt::Display for DesktopApp {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.version().filter(|v| !v.is_empty()) {
            Some(v) => write!(f, "{} {v}", self.name()),
            None => write!(f, "{}", self.name()),
        }
    }
}

fn expand(text: &str) -> String {
    win::expand_env(text.trim().trim_matches('"'))
}

/// "Claude_pzs8sxrjxfjjc!Claude"; paths have separators, app ids never do.
fn is_app_id(candidate: &str) -> bool {
    matches!(candidate.find('!'), Some(i) if i > 0) && !candidate.contains(['\\', '/', ':'])
}

/// Full name of the newest package of this family installed for the user.
fn installed_package(family: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;
    let wfamily = win::wide(family);
    let mut count = 0u32;
    let mut length = 0u32;
    unsafe {
        let r = GetPackagesByPackageFamily(wfamily.as_ptr(), &mut count, std::ptr::null_mut(), &mut length, std::ptr::null_mut());
        if r != ERROR_INSUFFICIENT_BUFFER || count == 0 {
            return None; // none installed, or not a valid family name
        }
        let mut names: Vec<*mut u16> = vec![std::ptr::null_mut(); count as usize];
        let mut buffer = vec![0u16; length as usize];
        if GetPackagesByPackageFamily(wfamily.as_ptr(), &mut count, names.as_mut_ptr(), &mut length, buffer.as_mut_ptr()) != 0 {
            return None;
        }
        String::from_utf16_lossy(&buffer)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .max_by_key(|n| package_version(n))
    }
}

fn package_version(full_name: &str) -> Vec<u32> {
    full_name.split('_').nth(1).map(|v| v.split('.').map(|p| p.parse().unwrap_or(0)).collect()).unwrap_or_default()
}

/// FileDescription (or ProductName) / ProductVersion (or FileVersion) from the exe's version resource.
fn exe_info(exe: &str, name: bool) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let wexe = win::wide(exe);
    unsafe {
        let size = GetFileVersionInfoSizeW(wexe.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(wexe.as_ptr(), 0, size, data.as_mut_ptr() as *mut _) == 0 {
            return None;
        }
        let query = |sub: &str| -> Option<String> {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            if VerQueryValueW(data.as_ptr() as *const _, win::wide(sub).as_ptr(), &mut p, &mut len) == 0 || p.is_null() || len == 0 {
                return None;
            }
            Some(win::from_wide(std::slice::from_raw_parts(p as *const u16, len as usize)).trim().to_string())
        };
        let mut langs: Vec<String> = Vec::new();
        let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(data.as_ptr() as *const _, win::wide("\\VarFileInfo\\Translation").as_ptr(), &mut p, &mut len) != 0 && len >= 4 {
            let pairs = std::slice::from_raw_parts(p as *const u16, len as usize / 2);
            for pair in pairs.chunks_exact(2) {
                langs.push(format!("{:04x}{:04x}", pair[0], pair[1]));
            }
        }
        langs.extend(["040904b0", "040904e4", "04090000"].iter().map(|s| s.to_string()));
        let keys: [&str; 2] = if name { ["FileDescription", "ProductName"] } else { ["ProductVersion", "FileVersion"] };
        for key in keys {
            for lang in &langs {
                if let Some(v) = query(&format!("\\StringFileInfo\\{lang}\\{key}")).filter(|v| !v.is_empty()) {
                    return Some(v);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_like_uri_absolute_uri() {
        assert_eq!(normalize_url("gemini.google.com/app").as_deref(), Some("https://gemini.google.com/app"));
        assert_eq!(normalize_url("  https://chatgpt.com ").as_deref(), Some("https://chatgpt.com/"));
        assert_eq!(normalize_url("HTTP://Claude.AI/new?x=1").as_deref(), Some("http://claude.ai/new?x=1"));
        assert_eq!(normalize_url("https://grok.com:443/").as_deref(), Some("https://grok.com/"));
        assert_eq!(normalize_url("ftp://x.org"), None);
        assert_eq!(normalize_url(""), None);
        assert_eq!(normalize_url("https://"), None);
        assert_eq!(normalize_url("a b"), None);
    }

    #[test]
    fn app_ids_and_commands() {
        assert!(is_app_id("Claude_pzs8sxrjxfjjc!Claude"));
        assert!(!is_app_id("C:\\x!y.exe"));
        assert!(!is_app_id("!x"));
        let s = PetSettings { program: "\"C:\\npm\\codex.cmd\"".into(), ..Default::default() };
        assert_eq!(program_command(&s, "app"), "codex app");
        assert_eq!(terminal_dir("C:\\"), "C:\\.");
        assert!(package_version("A_1.10.0.0_x64__p") > package_version("A_1.9.0.0_x64__p"));
    }
}
