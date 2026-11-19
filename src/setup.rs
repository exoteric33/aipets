//! aipets.exe --install / --uninstall and "Set up hooks" in the settings window: autostart plus the status
//! hooks of every installed agent (Claude Code, Codex, Hermes Agent, Cursor), all pointing at this exe.
//! Only aipets' own entries are touched, a file is only written when something changes, and the old
//! version is kept as <file>.bak-aipets.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use regex_lite::Regex;

use crate::app;
use crate::json::{self, Json, Obj};
use crate::launcher;
use crate::win;

pub struct Step {
    pub name: String,
    /// done or already so; false = skipped or failed
    pub ok: bool,
    pub text: String,
}

impl Step {
    pub fn new(name: &str, ok: bool, text: impl Into<String>) -> Step {
        Step { name: name.to_string(), ok, text: text.into() }
    }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}{}: {}", if self.ok { "✓ " } else { "– " }, self.name, self.text)
    }
}

fn user_profile() -> String {
    win::user_profile().to_string_lossy().into_owned()
}

pub fn claude_settings() -> String {
    win::combine(&user_profile(), r".claude\settings.json")
}

pub fn codex_home() -> String {
    match std::env::var("CODEX_HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => win::combine(&user_profile(), ".codex"),
    }
}

pub fn cursor_home() -> String {
    win::combine(&user_profile(), ".cursor")
}

pub fn hermes_home() -> String {
    if let Ok(h) = std::env::var("HERMES_HOME") {
        if !h.is_empty() {
            return h;
        }
    }
    let home = win::local_app_data().join("hermes").to_string_lossy().into_owned();
    if win::dir_exists(&home) { home } else { win::combine(&user_profile(), ".hermes") }
}

pub fn install(exe: &str) -> Vec<Step> {
    let mut steps = vec![match app::autostart::choose(true) {
        // installing again also takes back an earlier "no"
        Ok(()) => Step::new("Start with Windows", true, "on"),
        Err(e) => Step::new("Start with Windows", false, e),
    }];
    for source in ["claude", "codex", "hermes", "cursor"] {
        steps.push(hooks(source, exe, true));
    }
    steps
}

pub fn uninstall(exe: &str) -> Vec<Step> {
    let mut steps = vec![match app::autostart::set_enabled(false) {
        Ok(()) => Step::new("Start with Windows", true, "off"),
        Err(e) => Step::new("Start with Windows", false, e),
    }];
    for source in ["claude", "codex", "hermes", "cursor"] {
        steps.push(hooks(source, exe, false));
    }
    steps
}

/// Adds (install) or removes the status hooks of one agent.
pub fn hooks(source: &str, exe: &str, install: bool) -> Step {
    let result = match source {
        "claude" => claude(&claude_settings(), exe, install),
        "codex" => codex(&codex_home(), exe, install, true),
        "hermes" => hermes(&hermes_home(), exe, install),
        "cursor" => cursor(&cursor_home(), exe, install),
        _ => return Step::new(source, false, "this status source has no hooks"),
    };
    result.unwrap_or_else(|e| {
        app::log(&format!("setup {source}: {e}"));
        let name = match source {
            "claude" => "Claude Code",
            "codex" => "Codex",
            "cursor" => "Cursor",
            _ => "Hermes Agent",
        };
        Step::new(name, false, format!("Error: {e}"))
    })
}

// ------------------------------------------------------------------ Claude Code

// event, matcher (None = none), hook event word, "async" or a timeout
const CLAUDE_EVENTS: [(&str, Option<&str>, &str, &str); 6] = [
    ("UserPromptSubmit", None, "working", "10"),
    ("PostToolUse", Some("*"), "resume", "async"),
    ("PostToolUseFailure", Some("*"), "resume", "async"),
    ("Notification", Some("permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input"), "waiting", "10"),
    ("Stop", None, "done", "10"),
    ("SessionEnd", None, "end", "10"),
];

pub fn claude(settings_path: &str, exe: &str, install: bool) -> Result<Step, String> {
    let name = "Claude Code";
    if !win::dir_exists(&win::dir_name(settings_path)) {
        return Ok(Step::new(name, !install, "not installed"));
    }
    let mut groups = Vec::new();
    for (event, matcher, what, timing) in CLAUDE_EVENTS {
        let mut handler = Obj::new();
        handler.set("type", Json::str("command"));
        handler.set("command", Json::str(exe));
        handler.set("args", Json::Arr(vec![Json::str("--hook"), Json::str("claude"), Json::str(what)]));
        if timing == "async" {
            handler.set("async", Json::bool(true));
        } else {
            handler.set("timeout", Json::int(timing.parse().unwrap()));
        }
        groups.push((event.to_string(), group(matcher, handler)));
    }
    let changed = edit_json_hooks(settings_path, "claude", if install { Some(groups) } else { None }, None)?;
    Ok(Step::new(name, true, format!("{} ({})", outcome(changed, install), app::short_path(settings_path))))
}

// ------------------------------------------------------------------ Codex

// event, matcher, hook event word, timeout, async; Codex runs hook commands through Windows PowerShell
const CODEX_EVENTS: [(&str, Option<&str>, &str, i64, bool); 6] = [
    ("UserPromptSubmit", None, "working", 10, true),
    ("PermissionRequest", Some("*"), "waiting", 10, true),
    ("PostToolUse", Some("*"), "resume", 10, true),
    ("Stop", None, "done", 10, true),
    ("Interrupt", None, "idle", 3, true),
    ("SessionEnd", None, "end", 3, false),
];

pub fn codex(home: &str, exe: &str, install: bool, trust: bool) -> Result<Step, String> {
    let name = "Codex";
    if !win::dir_exists(home) {
        return Ok(Step::new(name, !install, "not installed"));
    }
    let path = win::combine(home, "hooks.json");
    let mut groups = Vec::new();
    for (event, matcher, what, timeout, is_async) in CODEX_EVENTS {
        let mut handler = Obj::new();
        handler.set("type", Json::str("command"));
        // without Out-Null PowerShell does not wait for the GUI exe
        handler.set("command", Json::str(&format!("& '{}' --hook codex {what} | Out-Null", exe.replace('\'', "''"))));
        handler.set("timeout", Json::int(timeout));
        if is_async {
            handler.set("async", Json::bool(true));
        }
        groups.push((event.to_string(), group(matcher, handler)));
    }
    let changed = edit_json_hooks(
        &path,
        "codex",
        if install { Some(groups) } else { None },
        Some("aipets: the pets on the desktop show whether Codex is working, needs you or is done"),
    )?;
    let place = format!(" ({})", app::short_path(&path));
    if !install {
        return Ok(Step::new(name, true, format!("{}{place}", outcome(changed, false))));
    }
    let problem = if trust { trust_codex_hooks(&path) } else { None };
    Ok(match problem {
        Some(p) => Step::new(
            name,
            false,
            format!(
                "{}{place}, but not approved ({p}). Open /hooks in Codex once and approve the aipets hooks.",
                outcome(changed, true)
            ),
        ),
        None => Step::new(name, true, format!("{} and approved{place}", outcome(changed, true))),
    })
}

/// Trusts aipets' hooks the way Codex' /hooks does: codex app-server (JSON-RPC over stdio), hooks/list for
/// key and current hash, config/batchWrite on hooks.state. None on success, otherwise the reason.
fn trust_codex_hooks(hooks_path: &str) -> Option<String> {
    let codex = launcher::resolve("codex", r"%APPDATA%\npm\codex.cmd", &std::env::var("PATH").unwrap_or_default())?;
    let codex = Some(codex);
    let Some(codex) = codex else { return Some("codex not found".into()) };
    let mut cmd = if win::ends_with_ignore_case(&codex, ".cmd") || win::ends_with_ignore_case(&codex, ".bat") {
        let mut c = Command::new("cmd.exe");
        c.raw_arg(format!("/d /s /c \"\"{codex}\" app-server\""));
        c
    } else {
        let mut c = Command::new(&codex);
        c.arg("app-server");
        c
    };
    cmd.current_dir(user_profile())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Some(format!("codex app-server: {e}")),
    };
    let lines: Arc<(Mutex<VecDeque<Option<String>>>, Condvar)> = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
    let errors = Arc::new(Mutex::new(String::new()));
    if let Some(out) = child.stdout.take() {
        let lines = lines.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let (q, cv) = &*lines;
                q.lock().unwrap().push_back(Some(line));
                cv.notify_all();
            }
            let (q, cv) = &*lines;
            q.lock().unwrap().push_back(None); // stdout closed
            cv.notify_all();
        });
    }
    if let Some(err) = child.stderr.take() {
        let errors = errors.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                let mut e = errors.lock().unwrap();
                if e.len() < 4000 {
                    e.push_str(&line);
                    e.push('\n');
                }
            }
        });
    }
    let mut stdin = child.stdin.take();
    let mut send = |json: &str| {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(format!("{json}\n").as_bytes());
            let _ = s.flush();
        }
    };
    let result = (|| -> Option<String> {
        send(r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"aipets","title":null,"version":"1"},"capabilities":null}}"#);
        if reply(&lines, 1).is_none() {
            app::log(&format!("codex app-server did not answer initialize: {}", errors.lock().unwrap().trim()));
            return Some("codex app-server does not respond".into());
        }
        send(r#"{"method":"initialized"}"#);
        send(&format!(r#"{{"id":2,"method":"hooks/list","params":{{"cwds":[{}]}}}}"#, json::quote(&user_profile())));
        let listed = reply(&lines, 2);
        let data = listed.as_ref().and_then(|l| l.get("result")).and_then(Json::obj).and_then(|r| r.get("data")).and_then(Json::arr);
        let Some(data) = data else { return Some("hooks/list returned no result".into()) };
        let mut value = String::new();
        let mut found = 0;
        let full = win::full_path(hooks_path);
        for entry in data {
            let hooks = entry.obj().and_then(|e| e.get("hooks")).and_then(Json::arr);
            for item in hooks.into_iter().flatten() {
                let Some(hook) = item.obj() else { continue };
                let (Some(source), Some(command)) = (hook.text("sourcePath"), hook.text("command")) else { continue };
                if !win::contains_ignore_case(&command, "aipets.exe") || !win::eq_ignore_case(&win::full_path(&source), &full) {
                    continue;
                }
                found += 1;
                if hook.text("trustStatus").as_deref() == Some("trusted") {
                    continue;
                }
                if !value.is_empty() {
                    value.push(',');
                }
                value.push_str(&format!(
                    "{}:{{\"trusted_hash\":{}}}",
                    json::quote(&hook.text("key").unwrap_or_default()),
                    json::quote(&hook.text("currentHash").unwrap_or_default())
                ));
            }
        }
        app::log(&format!(
            "codex trust: {found} aipets hooks listed, {}",
            if value.is_empty() { "all trusted" } else { "trusting the rest" }
        ));
        if found == 0 {
            return Some("Codex does not list the hooks; is the \"hooks\" feature off?".into());
        }
        if value.is_empty() {
            return None; // all trusted already
        }
        send(&format!(
            r#"{{"id":3,"method":"config/batchWrite","params":{{"edits":[{{"keyPath":"hooks.state","value":{{{value}}},"mergeStrategy":"upsert"}}],"reloadUserConfig":true}}}}"#
        ));
        let written = reply(&lines, 3)?;
        written.get("error").map(|e| format!("config/batchWrite: {}", json::write(e).trim()))
    })();
    // reply(...)? above returns None for "no answer": tell it apart from success
    let result = match result {
        None if reply_missing(&lines) => Some("config/batchWrite does not respond".into()),
        r => r,
    };
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                break;
            }
        }
    }
    result
}

/// True when the app-server closed its output without answering (the last reply came back empty).
fn reply_missing(lines: &Arc<(Mutex<VecDeque<Option<String>>>, Condvar)>) -> bool {
    matches!(lines.0.lock().unwrap().front(), Some(None))
}

/// The response with this id (notifications and server requests are skipped), or None once the
/// app-server has closed stdout or after 30 s.
fn reply(lines: &Arc<(Mutex<VecDeque<Option<String>>>, Condvar)>, id: i64) -> Option<Obj> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let wanted = id.to_string();
    let (q, cv) = &**lines;
    loop {
        let line = {
            let mut guard = q.lock().unwrap();
            loop {
                if let Some(front) = guard.front() {
                    if front.is_none() {
                        return None; // left in the queue, so later calls return at once too
                    }
                    break;
                }
                let left = deadline.checked_duration_since(Instant::now())?;
                let (g, timeout) = cv.wait_timeout(guard, left).unwrap();
                guard = g;
                if timeout.timed_out() && guard.is_empty() {
                    return None;
                }
            }
            guard.pop_front().flatten()?
        };
        let Ok(Json::Obj(msg)) = json::parse(&line) else { continue };
        if matches!(msg.get("id"), Some(Json::Val(raw)) if *raw == wanted) && msg.get("method").is_none() {
            return Some(msg);
        }
    }
}

// ------------------------------------------------------------------ Claude / Codex hook files

fn group(matcher: Option<&str>, handler: Obj) -> Obj {
    let mut g = Obj::new();
    if let Some(m) = matcher {
        g.set("matcher", Json::str(m));
    }
    g.set("hooks", Json::Arr(vec![Json::Obj(handler)]));
    g
}

fn is_ours(item: &Json, source: &str) -> bool {
    let Some(handler) = item.obj() else { return false };
    let Some(command) = handler.text("command") else { return false };
    if !win::contains_ignore_case(&command, "aipets.exe") {
        return false;
    }
    if command.contains(&format!("--hook {source}")) {
        return true;
    }
    match handler.get("args").and_then(Json::arr) {
        Some(args) => args.len() >= 2 && args[0].text().as_deref() == Some("--hook") && args[1].text().as_deref() == Some(source),
        None => false,
    }
}

fn read_or_empty(path: &str) -> Result<String, String> {
    if win::file_exists(path) { win::read_text(path).map_err(|e| e.to_string()) } else { Ok(String::new()) }
}

/// Settings file with "hooks": { Event: [ { matcher, hooks: [handler] } ] } (Claude Code settings.json,
/// Codex hooks.json): removes aipets' handlers for this source, then adds the desired groups where the old
/// ones were. True if the file changed.
pub fn edit_json_hooks(path: &str, source: &str, desired: Option<Vec<(String, Obj)>>, description: Option<&str>) -> Result<bool, String> {
    let original = read_or_empty(path)?;
    let empty = original.trim().is_empty();
    if empty && desired.is_none() {
        return Ok(false);
    }
    let mut root = if empty { Json::Obj(Obj::new()) } else { json::parse(&original)? };
    let Json::Obj(root_obj) = &mut root else { return Err(format!("{} contains no JSON object", win::file_name(path))) };
    let before = if empty { String::new() } else { json::write(&json::parse(&original)?) };
    if empty {
        if let Some(d) = description {
            root_obj.set("description", Json::str(d));
        }
    }
    if root_obj.get("hooks").map(|h| h.obj().is_none()).unwrap_or(false) {
        return Err("hooks must be a JSON object".into());
    }
    if root_obj.get("hooks").is_none() {
        if desired.is_none() {
            return Ok(false);
        }
        root_obj.set("hooks", Json::Obj(Obj::new()));
    }
    let hooks = root_obj.get_mut("hooks").and_then(Json::obj_mut).unwrap();
    let mut slots: HashMap<String, usize> = HashMap::new(); // event -> index of aipets' old group
    for (event, value) in hooks.items.iter_mut() {
        let Some(groups) = value.arr_mut() else { continue };
        let mut g = groups.len();
        while g > 0 {
            g -= 1;
            let Some(handlers) = groups[g].obj_mut().and_then(|o| o.get_mut("hooks")).and_then(Json::arr_mut) else { continue };
            let before_len = handlers.len();
            handlers.retain(|h| !is_ours(h, source));
            if handlers.len() == before_len {
                continue;
            }
            if handlers.is_empty() {
                groups.remove(g);
            }
            slots.insert(event.clone(), g);
        }
    }
    if let Some(desired) = desired {
        for (event, value) in desired {
            if hooks.get(&event).map(|v| v.arr().is_none()).unwrap_or(false) {
                return Err(format!("{event} must be a JSON list"));
            }
            if hooks.get(&event).is_none() {
                hooks.set(&event, Json::Arr(Vec::new()));
            }
            let groups = hooks.get_mut(&event).and_then(Json::arr_mut).unwrap();
            match slots.get(&event) {
                Some(&slot) if slot <= groups.len() => groups.insert(slot, Json::Obj(value)),
                _ => groups.push(Json::Obj(value)),
            }
        }
    }
    for event in slots.keys() {
        if matches!(hooks.get(event).and_then(Json::arr), Some(g) if g.is_empty()) {
            hooks.remove(event);
        }
    }
    if hooks.items.is_empty() {
        root_obj.remove("hooks");
    }
    let after = json::write(&root);
    if after == before {
        return Ok(false);
    }
    save(path, &original, &after, true)?;
    Ok(true)
}

// ------------------------------------------------------------------ Cursor

// Cursor names the event in the payload (hook_event_name). No postToolUse: Cursor waits for every hook,
// and each one starts Windows PowerShell (about half a second).
const CURSOR_EVENTS: [&str; 3] = ["beforeSubmitPrompt", "stop", "sessionEnd"];

/// The command of aipets' Cursor hooks: the bare exe path, exactly the "command" of aipets' Claude Code
/// hooks. Cursor runs hooks through Windows PowerShell: a path PowerShell would split gets single quotes.
pub fn cursor_command(exe: &str) -> String {
    let plain = !exe.is_empty() && exe.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '\\' | '-'));
    if plain { exe.to_string() } else { format!("'{}'", exe.replace('\'', "''")) }
}

pub fn cursor(home: &str, exe: &str, install: bool) -> Result<Step, String> {
    let name = "Cursor";
    if !win::dir_exists(home) {
        return Ok(Step::new(name, !install, "not installed"));
    }
    let path = win::combine(home, "hooks.json");
    let changed = edit_cursor_hooks(&path, if install { Some(cursor_command(exe)) } else { None })?;
    Ok(Step::new(name, true, format!("{} ({})", outcome(changed, install), app::short_path(&path))))
}

fn is_ours_in_cursor(item: &Json) -> bool {
    let Some(command) = item.obj().and_then(|o| o.text("command")) else { return false };
    win::contains_ignore_case(&command, "aipets.exe") && (!command.contains("--hook") || command.contains("--hook cursor"))
}

/// Cursor's hooks.json: { "version": 1, "hooks": { event: [ { "command": …, "timeout": … } ] } }. Removes
/// aipets' entries and, with a command, adds one per event where the old ones were. True if the file changed.
pub fn edit_cursor_hooks(path: &str, command: Option<String>) -> Result<bool, String> {
    let original = read_or_empty(path)?;
    let empty = original.trim().is_empty();
    if empty && command.is_none() {
        return Ok(false);
    }
    let mut root = if empty { Json::Obj(Obj::new()) } else { json::parse(&original)? };
    let Json::Obj(root_obj) = &mut root else { return Err(format!("{} contains no JSON object", win::file_name(path))) };
    let before = if empty { String::new() } else { json::write(&json::parse(&original)?) };
    if command.is_some() && root_obj.get("version").is_none() {
        root_obj.set("version", Json::int(1));
    }
    if root_obj.get("hooks").map(|h| h.obj().is_none()).unwrap_or(false) {
        return Err("hooks must be a JSON object".into());
    }
    if root_obj.get("hooks").is_none() {
        if command.is_none() {
            return Ok(false);
        }
        root_obj.set("hooks", Json::Obj(Obj::new()));
    }
    let hooks = root_obj.get_mut("hooks").and_then(Json::obj_mut).unwrap();
    let mut slots: HashMap<String, usize> = HashMap::new();
    for (event, value) in hooks.items.iter_mut() {
        let Some(entries) = value.arr_mut() else { continue };
        let mut i = entries.len();
        while i > 0 {
            i -= 1;
            if is_ours_in_cursor(&entries[i]) {
                entries.remove(i);
                slots.insert(event.clone(), i);
            }
        }
    }
    if let Some(command) = &command {
        for event in CURSOR_EVENTS {
            if hooks.get(event).map(|v| v.arr().is_none()).unwrap_or(false) {
                return Err(format!("{event} must be a JSON list"));
            }
            if hooks.get(event).is_none() {
                hooks.set(event, Json::Arr(Vec::new()));
            }
            let entries = hooks.get_mut(event).and_then(Json::arr_mut).unwrap();
            let mut hook = Obj::new();
            hook.set("command", Json::str(command));
            hook.set("timeout", Json::int(10));
            match slots.get(event) {
                Some(&slot) if slot <= entries.len() => entries.insert(slot, Json::Obj(hook)),
                _ => entries.push(Json::Obj(hook)),
            }
        }
    }
    for event in slots.keys() {
        if matches!(hooks.get(event).and_then(Json::arr), Some(e) if e.is_empty()) {
            hooks.remove(event);
        }
    }
    if hooks.items.is_empty() {
        root_obj.remove("hooks");
    }
    let after = json::write(&root);
    if after == before {
        return Ok(false);
    }
    save(path, &original, &after, true)?;
    Ok(true)
}

// ------------------------------------------------------------------ Hermes Agent

const HERMES_EVENTS: [(&str, &str); 5] = [
    ("pre_llm_call", "working"),
    ("pre_approval_request", "waiting"),
    ("post_approval_response", "resume"),
    ("on_session_end", "done"),
    ("on_session_finalize", "end"),
];

pub fn hermes(home: &str, exe: &str, install: bool) -> Result<Step, String> {
    let name = "Hermes Agent";
    let config = win::combine(home, "config.yaml");
    if !win::file_exists(&config) {
        return Ok(Step::new(
            name,
            !install,
            if win::dir_exists(home) { "config.yaml missing (start Hermes once)" } else { "not installed" },
        ));
    }
    let changed = edit_hermes_config(&config, exe, install)?;
    let approved = edit_hermes_allowlist(&win::combine(home, "shell-hooks-allowlist.json"), exe, install)?;
    let mut text = format!(
        "{}{} ({})",
        outcome(changed || approved, install),
        if install { " and approved" } else { "" },
        app::short_path(&config)
    );
    if changed {
        text.push_str(". A running Hermes gateway only picks this up after a restart");
    }
    Ok(Step::new(name, true, text))
}

fn hermes_command(exe: &str, what: &str) -> String {
    format!("\"{exe}\" --hook hermes {what}")
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

fn is_match(pattern: &str, text: &str) -> bool {
    re(pattern).is_match(text)
}

fn our_item() -> Regex {
    re(r"(?i)^(\s*)-\s*command:.*aipets\.exe.*--hook\s+hermes\b")
}

/// config.yaml, edited line by line (no YAML library): aipets' "- command:" items under the top-level
/// hooks: block are removed, then one item per event is put right under the event key (added if missing).
/// Every line keeps its own line ending (Hermes writes CRLF, hand edits often LF) and new lines copy the
/// block's indentation. True if changed.
pub fn edit_hermes_config(path: &str, exe: &str, install: bool) -> Result<bool, String> {
    let original = win::read_text(path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = original.split('\n').map(str::to_string).collect(); // a trailing \r stays part of its line
    let top_re = re(r"^hooks\s*:");
    if lines.iter().filter(|l| top_re.is_match(l)).count() > 1 {
        return Err("Several hooks blocks in config.yaml; please merge them into one block.".into());
    }
    let mut top: isize = lines.iter().position(|l| top_re.is_match(l)).map(|i| i as isize).unwrap_or(-1);
    if top >= 0 {
        let t = top as usize;
        if !is_match(r"^hooks:\s*(\{\s*\}|null|~)?\s*(#.*)?$", &lines[t]) {
            return Err("hooks in config.yaml must be an indented YAML block.".into());
        }
        if install {
            lines[t] = re(r"^(hooks:)\s*(\{\s*\}|null|~)").replace(&lines[t], "${1}").into_owned();
        }
        let end = block_end(&lines, t);
        for (event, _) in HERMES_EVENTS {
            let key_re = re(&format!(r"^\s+{event}:\s*"));
            let full_re = re(&format!(r"^\s+{event}:\s*(\[\s*\])?\s*(#.*)?$"));
            let keys: Vec<&String> = lines[t + 1..end].iter().filter(|l| key_re.is_match(l)).collect();
            if keys.len() > 1 || keys.iter().any(|l| !full_re.is_match(l)) {
                return Err(format!("{event} must appear once as an indented YAML list."));
            }
        }
    }
    if top < 0 && !install {
        return Ok(false);
    }
    if install && top >= 0 && hermes_up_to_date(&lines, top as usize, exe) {
        return Ok(false); // removing and adding again could only move our items behind foreign ones
    }
    let event_names: Vec<&str> = HERMES_EVENTS.iter().map(|e| e.0).collect();
    let ours = our_item();

    if top >= 0 {
        let t = top as usize;
        let mut end = block_end(&lines, t);
        let mut i = t + 1;
        while i < end {
            let Some(m) = ours.captures(&lines[i]) else {
                i += 1;
                continue;
            };
            // the item: its dash line plus deeper lines, but never the next list item
            let item_indent = m.get(1).map(|g| g.as_str().len()).unwrap_or(0);
            let mut j = i + 1;
            while j < end && !lines[j].trim().is_empty() && indent(&lines[j]) > item_indent && !lines[j].trim_start().starts_with('-') {
                j += 1;
            }
            lines.drain(i..j);
            end -= j - i;
        }
        if !install {
            // event keys of ours left without items go, and the whole block if nothing is left
            let key_re = re(r"^(\s+)([A-Za-z_]+):\s*$");
            let mut i = end;
            while i > t + 1 {
                i -= 1;
                let Some(key) = key_re.captures(&lines[i]) else { continue };
                if !event_names.contains(&&key[2]) {
                    continue;
                }
                let key_indent = key[1].len();
                let mut next = i + 1;
                while next < end && lines[next].trim().is_empty() {
                    next += 1;
                }
                if next >= end || !is_item_of(&lines[next], key_indent) {
                    lines.remove(i);
                    end -= 1;
                }
            }
            let nothing_left = lines[t + 1..end].iter().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
            if nothing_left {
                lines.drain(t..end);
                let mut tt = t;
                if tt > 0 && lines[tt - 1].starts_with("# aipets") {
                    tt -= 1;
                    lines.remove(tt);
                    if tt > 0 && lines[tt - 1].trim().is_empty() {
                        tt -= 1;
                        lines.remove(tt); // the blank line install put before the block
                    }
                }
                top = tt as isize;
                if lines.last().map(|l| l.ends_with('\r')).unwrap_or(false) {
                    lines.push(String::new()); // the block was the end of the file: end with a whole line break
                }
            }
        }
    }

    if install {
        if top < 0 {
            // new block at the end, in the file's line ending style
            let eol = if lines.iter().any(|l| l.ends_with('\r')) { "\r" } else { "" };
            let mut at = lines.len() - 1; // before the empty rest after the final line break
            if !lines[at].is_empty() {
                // the file did not end with a line break: give its last line one
                lines[at] = format!("{}{eol}", lines[at].trim_end_matches('\r'));
                lines.push(String::new());
                at += 1;
            }
            let mut block: Vec<String> = Vec::new();
            if at > 0 && !lines[at - 1].trim().is_empty() {
                block.push(eol.to_string());
            }
            block.push(format!(
                "# aipets: the pets on the desktop show whether Hermes is working, waiting for an approval or done{eol}"
            ));
            block.push(format!("hooks:{eol}"));
            for (event, what) in HERMES_EVENTS {
                block.push(format!("  {event}:{eol}"));
                block.extend(hermes_item(exe, what, 4, eol));
            }
            lines.splice(at..at, block);
        } else {
            let t = top as usize;
            let mut end = block_end(&lines, t);
            // indentation of event keys and of their items, as the block already uses them
            let mut key_indent = 2;
            let mut item_offset = 2;
            let any_key = re(r"^(\s+)[A-Za-z_]+:\s*$");
            for i in t + 1..end {
                let Some(m) = any_key.captures(&lines[i]) else { continue };
                key_indent = m[1].len();
                if i + 1 < end && lines[i + 1].trim_start().starts_with('-') && indent(&lines[i + 1]) >= key_indent {
                    item_offset = indent(&lines[i + 1]) - key_indent; // 0: "- " right under the key, as PyYAML writes it
                }
                break;
            }
            for (event, what) in HERMES_EVENTS {
                let full_re = re(&format!(r"^\s+{event}:\s*(\[\s*\])?\s*(#.*)?$"));
                let key = (t + 1..end).find(|&i| full_re.is_match(&lines[i]));
                if let Some(key) = key {
                    let eol = if lines[key].ends_with('\r') { "\r" } else { "" };
                    let stripped = re(r"\[\s*\]").replace_all(lines[key].trim_end_matches('\r'), "").into_owned();
                    lines[key] = format!("{stripped}{eol}");
                    // line up with the items already under this key
                    let listed = key + 1 < end && lines[key + 1].trim_start().starts_with('-') && indent(&lines[key + 1]) >= indent(&lines[key]);
                    let item_indent = if listed { indent(&lines[key + 1]) } else { indent(&lines[key]) + item_offset };
                    let item = hermes_item(exe, what, item_indent, eol);
                    let n = item.len();
                    lines.splice(key + 1..key + 1, item);
                    end += n;
                } else {
                    let mut at = end;
                    while at - 1 > t && lines[at - 1].trim().is_empty() {
                        at -= 1;
                    }
                    let eol = if lines[t].ends_with('\r') { "\r" } else { "" }; // "hooks:" always has a line break
                    let mut added = vec![format!("{}{event}:{eol}", " ".repeat(key_indent))];
                    added.extend(hermes_item(exe, what, key_indent + item_offset, eol));
                    if at == lines.len() {
                        // after the file's last line, which had no line break: the file ends with one now
                        lines[at - 1] = format!("{}{eol}", lines[at - 1].trim_end_matches('\r'));
                        added.push(String::new());
                    }
                    let n = added.len();
                    lines.splice(at..at, added);
                    end += n;
                }
            }
        }
    }

    let result = lines.join("\n");
    if result == original {
        return Ok(false);
    }
    save(path, &original, &result, false)?;
    Ok(true)
}

fn hermes_item(exe: &str, what: &str, indent: usize, eol: &str) -> Vec<String> {
    // single quotes: in double quotes YAML would read \U... in a Windows path as an escape
    vec![
        format!("{}- command: '{}'{eol}", " ".repeat(indent), hermes_command(exe, what).replace('\'', "''")),
        format!("{}timeout: 10{eol}", " ".repeat(indent + 2)),
    ]
}

/// True if the hooks block holds exactly one aipets item for this exe under each event key and no other.
fn hermes_up_to_date(lines: &[String], top: usize, exe: &str) -> bool {
    let end = block_end(lines, top);
    let ours = our_item();
    let mut seen: Vec<String> = Vec::new();
    for i in top + 1..end {
        let Some(m) = ours.captures(&lines[i]) else { continue };
        let owner = owner_key(lines, top, i, m.get(1).map(|g| g.as_str().len()).unwrap_or(0));
        let Some(owner) = owner else { return false };
        let Some((_, what)) = HERMES_EVENTS.iter().find(|e| e.0 == owner) else { return false };
        if seen.contains(&owner) || lines[i].trim() != hermes_item(exe, what, 0, "")[0] {
            return false;
        }
        seen.push(owner);
    }
    seen.len() == HERMES_EVENTS.len()
}

/// The key a list item at this line belongs to, or None if it is not directly under a key.
fn owner_key(lines: &[String], top: usize, item: usize, item_indent: usize) -> Option<String> {
    let key_re = re(r"^\s+([A-Za-z_]+):\s*(#.*)?$");
    let mut k = item;
    while k > top + 1 {
        k -= 1;
        let trimmed = lines[k].trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || indent(&lines[k]) > item_indent
            || (indent(&lines[k]) == item_indent && trimmed.starts_with('-'))
        {
            continue; // blank, comment, part of an earlier item, or an earlier item of the same list
        }
        return key_re.captures(&lines[k]).map(|c| c[1].to_string());
    }
    None
}

/// Whether a line below a key at this indentation is part of its value (deeper, or a "- " item at the same depth).
fn is_item_of(line: &str, key_indent: usize) -> bool {
    indent(line) > key_indent || (indent(line) == key_indent && line.trim_start().starts_with('-'))
}

fn block_end(lines: &[String], top: usize) -> usize {
    let mut i = top + 1;
    while i < lines.len() && (lines[i].is_empty() || lines[i].chars().next().unwrap().is_whitespace() || lines[i].starts_with('#')) {
        i += 1;
    }
    // trailing comment or blank lines belong to what follows
    while i - 1 > top && (lines[i - 1].trim().is_empty() || lines[i - 1].starts_with('#')) {
        i -= 1;
    }
    i
}

fn indent(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Hermes asks once per (event, command) before running a shell hook and records the answer in
/// shell-hooks-allowlist.json; this records aipets' approvals the same way (and drops stale ones).
pub fn edit_hermes_allowlist(path: &str, exe: &str, install: bool) -> Result<bool, String> {
    let original = read_or_empty(path)?;
    let empty = original.trim().is_empty();
    if empty && !install {
        return Ok(false);
    }
    let mut root = if empty { Json::Obj(Obj::new()) } else { json::parse(&original)? };
    let Json::Obj(root_obj) = &mut root else { return Err(format!("{} contains no JSON object", win::file_name(path))) };
    let before = if empty { String::new() } else { json::write(&json::parse(&original)?) };
    if root_obj.get("approvals").map(|a| a.arr().is_none()).unwrap_or(false) {
        return Err("approvals must be a JSON list".into());
    }
    if root_obj.get("approvals").is_none() {
        root_obj.set("approvals", Json::Arr(Vec::new()));
    }
    let approvals = root_obj.get_mut("approvals").and_then(Json::arr_mut).unwrap();
    let mut wanted: Vec<String> = if install {
        HERMES_EVENTS.iter().map(|(e, w)| format!("{e}\n{}", hermes_command(exe, w))).collect()
    } else {
        Vec::new()
    };
    approvals.retain(|item| {
        let Some(o) = item.obj() else { return true };
        let Some(command) = o.text("command") else { return true };
        if !win::contains_ignore_case(&command, "aipets.exe") || !command.contains("--hook hermes") {
            return true;
        }
        let pair = format!("{}\n{command}", o.text("event").unwrap_or_default());
        match wanted.iter().position(|w| *w == pair) {
            Some(i) => {
                wanted.remove(i); // already approved: keep it as it is
                true
            }
            None => false,
        }
    });
    let now = win::utc_iso(win::now_ticks());
    let mtime = if win::file_exists(exe) {
        std::fs::metadata(exe).and_then(|m| m.modified()).ok().map(|t| win::utc_iso(win::system_time_ticks(t)))
    } else {
        None
    };
    for pair in wanted {
        let (event, command) = pair.split_once('\n').unwrap();
        let mut entry = Obj::new();
        entry.set("approved_at", Json::str(&now));
        entry.set("command", Json::str(command));
        entry.set("event", Json::str(event));
        entry.set("script_mtime_at_approval", mtime.as_deref().map(Json::str).unwrap_or_else(Json::null));
        approvals.push(Json::Obj(entry));
    }
    let after = json::write(&root);
    if after == before {
        return Ok(false);
    }
    save(path, &original, &after, true)?;
    Ok(true)
}

// ------------------------------------------------------------------ helpers

fn outcome(changed: bool, install: bool) -> &'static str {
    match (install, changed) {
        (true, true) => "Hooks added",
        (true, false) => "Hooks already set up",
        (false, true) => "Hooks removed",
        (false, false) => "no aipets hooks",
    }
}

/// Writes the file, keeping the previous content as .bak-aipets. JSON written from scratch gets CRLF if
/// the old file had it.
fn save(path: &str, original: &str, text: &str, match_line_endings: bool) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    if !original.is_empty() {
        app::atomic_write(std::path::Path::new(&format!("{path}.bak-aipets")), original).map_err(io)?;
    }
    let text = if match_line_endings && original.contains("\r\n") {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        text.to_string()
    };
    std::fs::create_dir_all(win::dir_name(path)).map_err(io)?;
    app::atomic_write(std::path::Path::new(path), &text).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> String {
        let d = std::env::temp_dir().join(format!("aipets-setup-{}", app::unique_id()));
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn hermes_install_then_uninstall_restores_file() {
        let dir = scratch();
        let path = win::combine(&dir, "config.yaml");
        let original = "model: x\r\nhooks:\r\n  pre_llm_call:\r\n  - command: other\r\n    timeout: 5\r\ndisplay:\r\n  a: 1";
        std::fs::write(&path, original).unwrap();
        let exe = r"C:\Users\me\Desktop\aipets\aipets.exe";
        assert!(edit_hermes_config(&path, exe, true).unwrap());
        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(installed.contains("  - command: '\"C:\\Users\\me\\Desktop\\aipets\\aipets.exe\" --hook hermes working'\r\n    timeout: 10\r\n  - command: other"));
        assert!(!edit_hermes_config(&path, exe, true).unwrap(), "second install is a no-op");
        assert!(edit_hermes_config(&path, exe, false).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hermes_new_block_and_removal() {
        let dir = scratch();
        let path = win::combine(&dir, "config.yaml");
        std::fs::write(&path, "model: x\n").unwrap();
        assert!(edit_hermes_config(&path, "C:\\a\\aipets.exe", true).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("model: x\n\n# aipets: the pets"));
        assert!(text.ends_with("  on_session_finalize:\n    - command: '\"C:\\a\\aipets.exe\" --hook hermes end'\n      timeout: 10\n"));
        assert!(edit_hermes_config(&path, "C:\\a\\aipets.exe", false).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model: x\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_hooks_keep_foreign_ones_in_place() {
        let dir = scratch();
        let path = win::combine(&dir, "settings.json");
        let foreign = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x.exe"}]},{"hooks":[{"type":"command","command":"C:\\old\\aipets.exe","args":["--hook","claude","done"]}]}]},"model":"m"}"#;
        std::fs::write(&path, foreign).unwrap();
        let step = claude(&path, r"C:\new\aipets.exe", true).unwrap();
        assert!(step.ok && step.text.starts_with("Hooks added"));
        let text = std::fs::read_to_string(&path).unwrap();
        let v = json::parse(&text).unwrap();
        let stop = v.obj().unwrap().get("hooks").unwrap().obj().unwrap().get("Stop").unwrap().arr().unwrap();
        assert_eq!(stop.len(), 2);
        assert!(json::write(&stop[1]).contains(r"C:\\new\\aipets.exe"));
        assert!(!claude(&path, r"C:\new\aipets.exe", true).unwrap().text.starts_with("Hooks added"));
        claude(&path, r"C:\new\aipets.exe", false).unwrap();
        let v = json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json::write(&v), "{\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"x.exe\"\n          }\n        ]\n      }\n    ]\n  },\n  \"model\": \"m\"\n}\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cursor_commands_and_file() {
        assert_eq!(cursor_command(r"C:\Users\me\Desktop\aipets\aipets.exe"), r"C:\Users\me\Desktop\aipets\aipets.exe");
        assert_eq!(cursor_command(r"C:\My Apps\it's\aipets.exe"), r"'C:\My Apps\it''s\aipets.exe'");
        let dir = scratch();
        let path = win::combine(&dir, "hooks.json");
        assert!(edit_cursor_hooks(&path, Some("C:\\a\\aipets.exe".into())).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n  \"version\": 1,\n  \"hooks\": {\n    \"beforeSubmitPrompt\""));
        assert!(edit_cursor_hooks(&path, None).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\n  \"version\": 1\n}\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Comparison with another build: AIPETS_SETUP_DIFF="<dir>|<exe>|<1 = install>" edits the configs under
    /// <dir> (claude\settings.json, codex\, cursor\, hermes\) and writes the step texts to <dir>\steps.txt.
    #[test]
    #[ignore]
    fn setup_diff() {
        let spec = std::env::var("AIPETS_SETUP_DIFF").expect("AIPETS_SETUP_DIFF");
        let parts: Vec<&str> = spec.split('|').collect();
        let (dir, exe, install) = (parts[0], parts[1], parts[2] == "1");
        let text = |r: Result<Step, String>| match r {
            Ok(step) => step.to_string(),
            Err(e) => format!("error: {e}"),
        };
        let steps = [
            text(claude(&win::combine(dir, r"claude\settings.json"), exe, install)),
            text(codex(&win::combine(dir, "codex"), exe, install, false)),
            text(cursor(&win::combine(dir, "cursor"), exe, install)),
            text(hermes(&win::combine(dir, "hermes"), exe, install)),
        ];
        std::fs::write(win::combine(dir, "steps.txt"), steps.join("\n")).unwrap();
    }
}
