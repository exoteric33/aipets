//! Agent activity: the hook command the agents run (aipets.exe --hook <source> <event>) writes one file
//! per session to %APPDATA%\aipets\status\<source>\, and the pets fold those files into one state.
//! Line format: State|unix ms|agent pid|extra (Claude, Codex, Cursor: transcript path, Hermes: open turn ids)

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, STILL_ACTIVE};
use windows_sys::Win32::System::Threading::*;

use crate::app;
use crate::json;
use crate::win;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AgentState {
    #[default]
    Idle,
    Working,
    Waiting,
    Done,
}

impl AgentState {
    pub fn name(self) -> &'static str {
        match self {
            AgentState::Idle => "Idle",
            AgentState::Working => "Working",
            AgentState::Waiting => "Waiting",
            AgentState::Done => "Done",
        }
    }

    fn parse(s: &str) -> Option<AgentState> {
        match s.trim() {
            "Idle" | "0" => Some(AgentState::Idle),
            "Working" | "1" => Some(AgentState::Working),
            "Waiting" | "2" => Some(AgentState::Waiting),
            "Done" | "3" => Some(AgentState::Done),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StatusEntry {
    pub state: AgentState,
    pub time: i64,
    pub pid: i32,
    pub extra: String,
}

impl StatusEntry {
    pub fn folder(source: &str) -> PathBuf {
        app::data_dir().join("status").join(source)
    }

    pub fn read(path: &Path) -> Option<StatusEntry> {
        let bytes = std::fs::read(path).ok()?;
        let text = win::decode_text(&bytes);
        let p: Vec<&str> = text.trim().splitn(4, '|').collect();
        if p.len() < 4 {
            return None;
        }
        Some(StatusEntry {
            state: AgentState::parse(p[0])?,
            time: p[1].trim().parse().ok()?,
            pid: app::parse_int(p[2])?,
            extra: p[3].to_string(),
        })
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), win::current_pid()));
        std::fs::write(&tmp, format!("{}|{}|{}|{}", self.state.name(), self.time, self.pid, self.extra))?;
        let mut attempt = 1;
        loop {
            match std::fs::rename(&tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt == 3 {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e);
                    }
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(25)); // a pet may be reading the file this instant
                }
            }
        }
    }
}

// ------------------------------------------------------------------ hook command

/// aipets.exe --hook <source> <event>, with the agent's hook JSON on stdin. Prints nothing.
pub fn hook_run(source: &str, what: Option<&str>) {
    app::set_log_tag(&format!("hook {source}"));
    let result: Result<(), String> = (|| {
        if source == "cursor" {
            let pid = cursor_pid(); // first: PowerShell, the hook's parent, may be gone soon
            let payload = read_stdin();
            let step = what.map(str::to_string).or_else(|| cursor_step(&payload));
            return cursor(step.as_deref().unwrap_or(""), &payload, pid);
        }
        // always drain stdin: the agent may still be writing and would see a broken pipe
        let input = read_stdin();
        match source {
            "claude" | "codex" => session(source, what.unwrap_or(""), &input, find_ancestor(source, 6)),
            "hermes" => hermes(what.unwrap_or(""), &input),
            _ => {
                app::log("unknown hook source");
                Ok(())
            }
        }
    })();
    if let Err(e) = result {
        app::log(&format!("{}: {e}", what.unwrap_or("")));
    }
}

fn read_stdin() -> String {
    let mut bytes = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut bytes);
    win::decode_text(&bytes)
}

fn stdin_redirected() -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_CHAR, FILE_TYPE_UNKNOWN};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        if h.is_null() || h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return false;
        }
        let t = GetFileType(h);
        t != FILE_TYPE_CHAR && t != FILE_TYPE_UNKNOWN
    }
}

/// Cursor starts aipets.exe without arguments: aipets' own Cursor hooks look like that, and Cursor also
/// runs Claude Code's hooks but drops their "args". Cursor names the event in the payload.
/// True if this process was such a hook and has done its work; false for a normal start.
pub fn run_cursor_hook() -> bool {
    // Cursor sets CURSOR_VERSION only for hooks and always pipes the payload in
    if std::env::var_os("CURSOR_VERSION").is_none() || !stdin_redirected() {
        return false;
    }
    let pid = cursor_pid();
    // never hang on a stdin that is not closed: the payload comes at once
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_stdin());
    });
    let Ok(payload) = rx.recv_timeout(std::time::Duration::from_secs(5)) else { return false };
    if !payload.trim_start().starts_with('{') {
        return false;
    }
    app::set_log_tag("hook cursor");
    let step = cursor_step(&payload).unwrap_or_default();
    if let Err(e) = cursor(&step, &payload, pid) {
        app::log(&format!("cursor: {e}"));
    }
    true // a payload from Cursor, even for an event aipets does not use: never start the tray from a hook
}

/// The Cursor event of a hook payload (hook_event_name), or None.
pub fn cursor_step(payload: &str) -> Option<String> {
    json::parse(payload).ok()?.obj()?.text("hook_event_name")
}

/// The Cursor window that ran the hook: Cursor's main process sets VSCODE_PID, which every hook inherits.
/// Otherwise the nearest Cursor.exe above the hook, or 0.
fn cursor_pid() -> i32 {
    if let Some(pid) = std::env::var("VSCODE_PID").ok().and_then(|v| app::parse_int(&v)) {
        if is_image(pid, "cursor") {
            return pid;
        }
    }
    find_ancestor("cursor", 6)
}

/// Cursor, one file per conversation: beforeSubmitPrompt → working, stop → done if completed (aborted or
/// failed turns end quietly), sessionEnd → end. postToolUse (from Claude Code's imported hooks) refreshes a
/// working turn. Cursor has no event for "waits for your approval". Other events change nothing.
pub fn cursor(step: &str, payload: &str, pid: i32) -> Result<(), String> {
    let what = match step {
        "beforeSubmitPrompt" => "working",
        "postToolUse" => "resume",
        "stop" => {
            let status = json::parse(payload).ok().and_then(|v| v.obj().and_then(|o| o.text("status")));
            if status.as_deref() == Some("completed") { "done" } else { "idle" }
        }
        "sessionEnd" => "end",
        _ => return Ok(()),
    };
    session("cursor", what, payload, pid)
}

fn invalid_file_name(id: &str) -> bool {
    id.chars().any(|c| (c as u32) < 32 || matches!(c, '"' | '<' | '>' | '|' | ':' | '*' | '?' | '\\' | '/'))
}

fn lock_status(key: &str, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    win::with_mutex(&format!(r"Local\aipets.status.{key}"), 3000, f).unwrap_or(Ok(()))
}

/// Claude Code and Codex, one file per session_id; Cursor, one per conversation_id.
/// Claude Code: UserPromptSubmit → working, PostToolUse → resume, Notification → waiting, Stop → done, SessionEnd → end.
/// Codex: UserPromptSubmit → working, PostToolUse → resume, PermissionRequest → waiting, Stop → done,
/// Interrupt → idle, SessionEnd → end. pid: the agent process; the pet drops the file once it is gone.
fn session(source: &str, what: &str, input: &str, pid: i32) -> Result<(), String> {
    let id = if source == "cursor" {
        json_string(input, "conversation_id").or_else(|| json_string(input, "session_id"))
    } else {
        json_string(input, "session_id")
    };
    let Some(id) = id.filter(|i| !i.is_empty() && !invalid_file_name(i)) else { return Ok(()) };
    let path = StatusEntry::folder(source).join(format!("{id}.txt"));
    if what == "end" {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    // the agents spawn hooks in event order; async ones may finish out of order, so a hook never
    // overwrites a state recorded by a later-spawned one (the mutex keeps reading and writing together)
    let time = process_start_unix_ms();
    lock_status(&format!("{source}-{id}"), || {
        let current = StatusEntry::read(&path);
        if matches!(&current, Some(c) if c.time > time) {
            return Ok(());
        }
        let state = match what {
            "working" => AgentState::Working,
            "waiting" => AgentState::Waiting,
            "done" => AgentState::Done,
            "idle" => AgentState::Idle, // Codex: Esc interrupted the turn
            "resume" => {
                // a tool finished: ends a permission wait, but must not revive a finished turn (background
                // agents keep running tools after Stop). Codex and Cursor also refresh a working turn.
                if let Some(c) = &current {
                    if c.state != AgentState::Waiting && !(source != "claude" && c.state == AgentState::Working) {
                        return Ok(());
                    }
                }
                AgentState::Working
            }
            _ => return Ok(()),
        };
        StatusEntry { state, time, pid, extra: json_string(input, "transcript_path").unwrap_or_default() }
            .write(&path)
            .map_err(|e| e.to_string())
    })
}

/// Hermes Agent shell hooks: pre_llm_call → working, pre_approval_request → waiting,
/// post_approval_response → resume, on_session_end → done, on_session_finalize → end.
/// One file per Hermes process; subagents open their own turns inside it, and the pet only shows
/// "done" once every open turn has ended.
fn hermes(what: &str, input: &str) -> Result<(), String> {
    let pid = parent_pid(unsafe { GetCurrentProcess() });
    if pid <= 0 {
        return Ok(());
    }
    let key = format!("hermes-{pid}");
    let path = StatusEntry::folder("hermes").join(format!("{key}.txt"));
    lock_status(&key, || {
        if what == "end" {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let current = StatusEntry::read(&path);
        let mut state = current.as_ref().map(|c| c.state).unwrap_or(AgentState::Idle);
        let mut turns: Vec<String> = current
            .as_ref()
            .map(|c| c.extra.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        let mut turn = json_string(input, "turn_id").unwrap_or_default().replace([',', '|'], "");
        match what {
            "working" => {
                if turn.is_empty() {
                    turn = "?".into();
                }
                if !turns.contains(&turn) {
                    turns.push(turn);
                }
                if state != AgentState::Waiting {
                    state = AgentState::Working;
                }
            }
            "waiting" => state = AgentState::Waiting,
            "resume" => {
                if state != AgentState::Waiting {
                    return Ok(());
                }
                state = if turns.is_empty() { AgentState::Idle } else { AgentState::Working };
            }
            "done" => {
                if turn.is_empty() {
                    turns.clear();
                } else if let Some(i) = turns.iter().position(|t| *t == turn) {
                    turns.remove(i);
                } else if let Some(i) = turns.iter().position(|t| t == "?") {
                    turns.remove(i);
                }
                if turns.is_empty() {
                    // interrupted or failed turns end quietly
                    state = if json_bool(input, "completed") { AgentState::Done } else { AgentState::Idle };
                }
            }
            _ => return Ok(()),
        }
        StatusEntry { state, time: win::unix_now_ms(), pid, extra: turns.join(",") }.write(&path).map_err(|e| e.to_string())
    })
}

/// First "name": "value" in the JSON. Hook payloads put their ids before any user content.
pub fn json_string(json: &str, name: &str) -> Option<String> {
    let re = regex_lite::Regex::new(&format!(r#""{}"\s*:\s*"((?:[^"\\]|\\.)*)""#, regex_lite::escape(name))).ok()?;
    let s = re.captures(json)?.get(1)?.as_str();
    let u: Vec<u16> = s.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        if u[i] != b'\\' as u16 || i + 1 >= u.len() {
            out.push(u[i]);
            i += 1;
            continue;
        }
        i += 1;
        match u[i] {
            0x6E => out.push(b'\n' as u16),
            0x74 => out.push(b'\t' as u16),
            0x72 => out.push(b'\r' as u16),
            0x75 => {
                if i + 4 < u.len() {
                    let hex = String::from_utf16_lossy(&u[i + 1..i + 5]);
                    out.push(u16::from_str_radix(&hex, 16).unwrap_or(0xFFFD));
                    i += 4;
                }
            }
            c => out.push(c), // \\ \" \/
        }
        i += 1;
    }
    Some(String::from_utf16_lossy(&out))
}

fn json_bool(json: &str, name: &str) -> bool {
    regex_lite::Regex::new(&format!(r#""{}"\s*:\s*(true|false)"#, regex_lite::escape(name)))
        .ok()
        .and_then(|re| re.captures(json).map(|c| &c[1] == "true"))
        .unwrap_or(false)
}

// ------------------------------------------------------------------ monitor

/// Folds all status files of one source into one state for the pet: waiting > working > idle.
pub struct StatusMonitor {
    folder: PathBuf,
    claude_transcripts: bool,
    codex: bool,
    transcripts: bool,
    background: bool,
    stale_ms: i64,
    checked_transcripts: HashMap<String, i64>,
    codex_questions: HashMap<String, Arc<CodexQuestions>>,
    pub state: AgentState,
    pub latest_done: i64,
}

const TAIL_BYTES: u64 = 16 * 1024;

impl StatusMonitor {
    pub fn new(source: &str, background: bool) -> StatusMonitor {
        let claude = source == "claude";
        let codex = source == "codex";
        StatusMonitor {
            folder: StatusEntry::folder(source),
            claude_transcripts: claude,
            codex,
            transcripts: claude || codex || source == "cursor", // extra holds the transcript path
            background,
            // Claude's transcript and Codex's tool hooks show activity, so a quiet session is stale soon;
            // Hermes and Cursor always report the end of a turn, the timeout only catches lost hooks
            stale_ms: if claude || codex { 15 * 60 * 1000 } else { 2 * 3600 * 1000 },
            checked_transcripts: HashMap::new(),
            codex_questions: HashMap::new(),
            state: AgentState::Idle,
            latest_done: 0,
        }
    }

    pub fn scan(&mut self) {
        let mut working = false;
        let mut waiting = false;
        let mut latest_done = 0i64;
        let now = win::unix_now_ms();
        let files: Vec<PathBuf> = std::fs::read_dir(&self.folder)
            .map(|d| {
                d.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && win::ends_with_ignore_case(&p.to_string_lossy(), ".txt"))
                    .collect()
            })
            .unwrap_or_default();
        for path in files {
            let Some(mut s) = StatusEntry::read(&path) else { continue };
            let gone = if s.pid > 0 { !is_alive(s.pid) } else { now - s.time > 12 * 3600 * 1000 };
            if gone {
                let _ = std::fs::remove_file(&path); // terminal closed without a session-end hook
                continue;
            }
            let mut questions_current = true;
            if self.codex && self.codex_asks(&s, &mut questions_current) {
                // a question stays open through Stop and, after the async variant, while Codex keeps working
                waiting = true;
                continue;
            }
            if s.state == AgentState::Working || s.state == AgentState::Waiting {
                let activity = if self.transcripts { s.time.max(transcript_time(&s.extra)) } else { s.time };
                if (self.claude_transcripts && self.interrupted(&s)) || now - activity > self.stale_ms {
                    // Esc in Claude Code does not fire Stop, a failed Codex turn neither; don't spin forever
                    s.state = AgentState::Idle;
                    if !self.codex {
                        s.time = now;
                        let _ = s.write(&path);
                    }
                    // Codex keeps its file: the next tool hook of a long-running turn brings the spinner back
                }
            }
            match s.state {
                AgentState::Working => working = true,
                AgentState::Waiting => waiting = true,
                // no completion flash before a newly queued question read has caught up to this hook
                AgentState::Done if questions_current => latest_done = latest_done.max(s.time),
                _ => {}
            }
        }
        self.state = if waiting {
            AgentState::Waiting
        } else if working {
            AgentState::Working
        } else {
            AgentState::Idle
        };
        self.latest_done = latest_done;
    }

    /// The newest message in the transcript is Claude Code's "[Request interrupted by user…]" marker.
    fn interrupted(&mut self, s: &StatusEntry) -> bool {
        let written = transcript_time(&s.extra);
        if written <= s.time || self.checked_transcripts.get(&s.extra) == Some(&written) {
            return false;
        }
        self.checked_transcripts.insert(s.extra.clone(), written);
        let Ok(mut f) = std::fs::File::open(&s.extra) else { return false };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).is_err() {
            return false;
        }
        let mut bytes = Vec::new();
        if f.read_to_end(&mut bytes).is_err() {
            return false;
        }
        let tail = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = tail.split('\n').collect();
        for line in lines.iter().skip(1).rev() {
            // line 0 may be cut off
            if line.contains("\"type\":\"assistant\"") {
                return false;
            }
            if line.contains("\"type\":\"user\"") {
                return line.contains("[Request interrupted by user");
            }
        }
        false
    }

    /// Codex asks through its request_user_input tools, which fire no hook, and with
    /// --dangerously-bypass-approvals-and-sandbox PermissionRequest never fires either. So the rollout
    /// transcript decides: request_user_input (Plan mode) is open until its function_call_output,
    /// request_user_input_async returns at once and is open until the next turn starts.
    fn codex_asks(&mut self, status: &StatusEntry, current: &mut bool) -> bool {
        *current = true;
        let transcript = &status.extra;
        if transcript.is_empty() {
            return false;
        }
        let q = self.codex_questions.entry(transcript.clone()).or_insert_with(|| Arc::new(CodexQuestions::default())).clone();
        let (asks, hook_time) = q.poll(transcript, status.time, self.background);
        *current = hook_time >= status.time;
        asks
    }
}

fn transcript_time(transcript: &str) -> i64 {
    if transcript.is_empty() {
        return 0;
    }
    win::file_mtime_unix_ms(transcript)
}

/// Reads each rollout byte once, off the animation thread, and remembers open questions.
#[derive(Default)]
struct CodexQuestions {
    reading: AtomicBool,
    inner: Mutex<CodexReader>,
    result: Mutex<(bool, i64)>,
}

#[derive(Default)]
struct CodexReader {
    pending: HashSet<String>,
    line: Vec<u8>,
    offset: u64,
    written: u64,
    async_question: bool,
}

impl CodexQuestions {
    fn poll(self: &Arc<Self>, transcript: &str, hook_time: i64, background: bool) -> (bool, i64) {
        if self.reading.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let me = self.clone();
            let path = transcript.to_string();
            let read = move || {
                let _ = me.read(&path, hook_time);
                me.reading.store(false, Ordering::Release);
            };
            if background {
                std::thread::spawn(read);
            } else {
                read(); // --status diagnostics need the result before exiting
            }
        }
        *self.result.lock().unwrap()
    }

    fn read(&self, transcript: &str, hook_time: i64) -> std::io::Result<()> {
        let mut r = self.inner.lock().unwrap();
        let mut f = std::fs::File::open(transcript)?;
        let length = f.metadata()?.len(); // one snapshot; do not chase a continuously growing rollout
        let stamp = win::file_stamp(Path::new(transcript));
        if length < r.offset || (length == r.offset && r.written != 0 && stamp != r.written) {
            r.offset = 0;
            r.line.clear();
            r.pending.clear();
            r.async_question = false;
        }
        f.seek(SeekFrom::Start(r.offset))?;
        let mut buffer = vec![0u8; 16 * 1024];
        while r.offset < length {
            let want = buffer.len().min((length - r.offset) as usize);
            let count = f.read(&mut buffer[..want])?;
            if count == 0 {
                return Ok(()); // truncated during the read: retry, retaining the published state
            }
            let mut start = 0;
            for i in 0..count {
                if buffer[i] != b'\n' {
                    continue;
                }
                r.line.extend_from_slice(&buffer[start..i]);
                let text = String::from_utf8_lossy(&r.line).into_owned();
                apply_line(&mut r, &text);
                r.line.clear();
                if r.line.capacity() > buffer.len() {
                    r.line = Vec::new();
                }
                start = i + 1;
            }
            // keep an unfinished record as bytes, including any split UTF-8 character
            r.line.extend_from_slice(&buffer[start..count]);
            r.offset += count as u64; // only successfully read bytes are committed; failures retry here
        }
        r.written = stamp;
        // publish together, only after catching up: no historic questions or premature completion
        *self.result.lock().unwrap() = (r.async_question || !r.pending.is_empty(), hook_time);
        Ok(())
    }
}

fn apply_line(r: &mut CodexReader, text: &str) {
    if text.contains("\"type\":\"task_started\"") {
        r.pending.clear();
        r.async_question = false;
    } else if text.contains("\"type\":\"function_call_output\"") {
        if !r.pending.is_empty() {
            r.pending.remove(&call_id(text));
        }
    } else if text.contains("\"type\":\"function_call\"") {
        if text.contains("\"name\":\"request_user_input_async\"") {
            r.async_question = true;
        } else if text.contains("\"name\":\"request_user_input\"") {
            r.pending.insert(call_id(text));
        }
    }
}

fn call_id(text: &str) -> String {
    let Some(i) = text.find("\"call_id\":\"") else { return String::new() };
    let rest = &text[i + 11..];
    match rest.find('"') {
        Some(0) | None => String::new(),
        Some(j) => rest[..j].to_string(),
    }
}

// ------------------------------------------------------------------ processes

#[repr(C)]
struct ProcessBasicInformation {
    exit_status: isize,
    peb: *mut std::ffi::c_void,
    affinity: usize,
    base_priority: isize,
    pid: usize,
    parent: usize,
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut std::ffi::c_void, len: u32, ret: *mut u32) -> i32;
}

fn parent_pid(process: HANDLE) -> i32 {
    let mut info: ProcessBasicInformation = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            0,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<ProcessBasicInformation>() as u32,
            &mut ret,
        )
    };
    if status != 0 { 0 } else { info.parent as i32 }
}

fn open(pid: i32) -> HANDLE {
    if pid <= 0 {
        return std::ptr::null_mut();
    }
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) }
}

fn has_image(process: HANDLE, image: &str) -> bool {
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size) } == 0 {
        return false;
    }
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    win::eq_ignore_case(win::file_stem(&path), image)
}

/// Nearest ancestor process with this image name (e.g. the claude.exe that ran the hook), or 0.
pub fn find_ancestor(image: &str, max_depth: usize) -> i32 {
    let mut pid = parent_pid(unsafe { GetCurrentProcess() });
    for _ in 0..max_depth {
        if pid <= 0 {
            break;
        }
        let h = open(pid);
        if h.is_null() {
            return 0;
        }
        let found = has_image(h, image);
        let next = parent_pid(h);
        unsafe { CloseHandle(h) };
        if found {
            return pid;
        }
        pid = next;
    }
    0
}

/// The process runs and its exe has this name (without .exe).
pub fn is_image(pid: i32, image: &str) -> bool {
    let h = open(pid);
    if h.is_null() {
        return false;
    }
    let mut code = 0u32;
    let r = unsafe { GetExitCodeProcess(h, &mut code) } != 0 && code == STILL_ACTIVE as u32 && has_image(h, image);
    unsafe { CloseHandle(h) };
    r
}

pub fn is_alive(pid: i32) -> bool {
    let h = open(pid);
    if h.is_null() {
        return false;
    }
    let mut code = 0u32;
    let r = unsafe { GetExitCodeProcess(h, &mut code) } != 0 && code == STILL_ACTIVE as u32;
    unsafe { CloseHandle(h) };
    r
}

/// When this process started, in unix ms (orders hooks that finish out of order).
fn process_start_unix_ms() -> i64 {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    win::ticks_to_unix_ms(win::filetime_ticks(&created))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_scanning() {
        let payload = r#"{"session_id":"ab\"c","nested":{"session_id":"x"},"transcript_path":"C:\\Users\\a\\t.jsonl","completed": true}"#;
        assert_eq!(json_string(payload, "session_id").as_deref(), Some("ab\"c"));
        assert_eq!(json_string(payload, "transcript_path").as_deref(), Some("C:\\Users\\a\\t.jsonl"));
        assert_eq!(json_string(payload, "missing"), None);
        assert!(json_bool(payload, "completed"));
        assert!(!json_bool(payload, "other"));
        assert_eq!(json_string(r#"{"a":"\u00e9\n"}"#, "a").as_deref(), Some("\u{e9}\n"));
    }

    #[test]
    fn codex_lines() {
        let mut r = CodexReader::default();
        apply_line(&mut r, r#"{"type":"function_call","name":"request_user_input","call_id":"c1"}"#);
        assert!(r.pending.contains("c1"));
        apply_line(&mut r, r#"{"type":"function_call_output","call_id":"c1"}"#);
        assert!(r.pending.is_empty());
        apply_line(&mut r, r#"{"type":"function_call","name":"request_user_input_async","call_id":"c2"}"#);
        assert!(r.async_question);
        apply_line(&mut r, r#"{"type":"task_started"}"#);
        assert!(!r.async_question);
        assert_eq!(call_id(r#""call_id":"""#), "");
    }

    #[test]
    fn entries_roundtrip() {
        let dir = std::env::temp_dir().join(format!("aipets-test-{}", app::unique_id()));
        let path = dir.join("s.txt");
        let e = StatusEntry { state: AgentState::Waiting, time: 123, pid: 45, extra: "a|b".into() };
        e.write(&path).unwrap();
        let r = StatusEntry::read(&path).unwrap();
        assert_eq!((r.state, r.time, r.pid, r.extra.as_str()), (AgentState::Waiting, 123, 45, "a|b"));
        // files of the C# version start with a BOM
        std::fs::write(&path, "\u{feff}Done|5|0|").unwrap();
        assert_eq!(StatusEntry::read(&path).unwrap().state, AgentState::Done);
        let _ = std::fs::remove_dir_all(dir);
    }
}
