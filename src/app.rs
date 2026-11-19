//! Paths, settings.ini, the log and autostart: shared by the tray, the pets and the hook command.
//! No windows here, so the hook path stays fast.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::win;

pub const NAME: &str = "aipets";

pub fn exe_path() -> &'static str {
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        let p = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "aipets.exe".into());
        p.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(p)
    })
}

pub fn dir() -> String {
    win::dir_name(exe_path())
}

pub fn pets_dir() -> String {
    win::combine(&dir(), "pets")
}

/// %APPDATA%\aipets, or AIPETS_DATA for tests (own settings, log and status files).
pub fn data_dir() -> PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| match std::env::var_os("AIPETS_DATA") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => win::roaming_app_data().join(NAME),
    })
    .clone()
}

// ------------------------------------------------------------------ ini

/// Minimal ini: [sections] with key=value lines, ';' and '#' start comments. Names ignore case.
/// Keeps the order of sections and keys; a key added after a removal takes the freed place,
/// so files are written back exactly as the earlier C# version wrote them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ini {
    sections: Vec<Section>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Section {
    name: String,
    entries: Vec<Option<(String, String)>>,
    free: Vec<usize>,
}

impl Ini {
    pub fn new() -> Ini {
        Ini::default()
    }

    pub fn parse(text: &str, default_section: &str) -> Ini {
        let mut ini = Ini::new();
        let mut section = default_section.to_string();
        for raw in text.split('\n') {
            let line = raw.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
                section = line[1..line.len() - 1].trim().to_string();
                continue;
            }
            if let Some(eq) = line.find('=') {
                if eq > 0 {
                    ini.set(&section, line[..eq].trim(), Some(line[eq + 1..].trim()));
                }
            }
        }
        ini
    }

    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| win::eq_ignore_case(&s.name, name))
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?
            .entries
            .iter()
            .flatten()
            .find(|(k, _)| win::eq_ignore_case(k, key))
            .map(|(_, v)| v.as_str())
    }

    /// int.TryParse with a fallback.
    pub fn get_int(&self, section: &str, key: &str, fallback: i32) -> i32 {
        self.get(section, key).and_then(parse_int).unwrap_or(fallback)
    }

    pub fn section_names(&self) -> Vec<String> {
        self.sections.iter().map(|s| s.name.clone()).collect()
    }

    /// Sets a value; None removes the key.
    pub fn set(&mut self, section: &str, key: &str, value: Option<&str>) {
        let idx = match self.sections.iter().position(|s| win::eq_ignore_case(&s.name, section)) {
            Some(i) => i,
            None => {
                if value.is_none() {
                    return;
                }
                self.sections.push(Section { name: section.to_string(), ..Default::default() });
                self.sections.len() - 1
            }
        };
        let s = &mut self.sections[idx];
        let found = s.entries.iter().position(|e| matches!(e, Some((k, _)) if win::eq_ignore_case(k, key)));
        match (found, value) {
            (Some(i), Some(v)) => s.entries[i].as_mut().unwrap().1 = v.to_string(),
            (Some(i), None) => {
                s.entries[i] = None;
                s.free.push(i);
            }
            (None, Some(v)) => {
                let entry = Some((key.to_string(), v.to_string()));
                match s.free.pop() {
                    Some(i) => s.entries[i] = entry,
                    None => s.entries.push(entry),
                }
            }
            (None, None) => {}
        }
    }

    pub fn format(&self) -> String {
        let mut text = String::new();
        for s in &self.sections {
            if s.entries.iter().all(Option::is_none) {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\r\n");
            }
            text.push('[');
            text.push_str(&s.name);
            text.push_str("]\r\n");
            for (k, v) in s.entries.iter().flatten() {
                text.push_str(k);
                text.push('=');
                text.push_str(v);
                text.push_str("\r\n");
            }
        }
        text
    }
}

/// int.TryParse (invariant): optional sign, digits, surrounding white space.
pub fn parse_int(s: &str) -> Option<i32> {
    let t = s.trim();
    let t = t.strip_prefix('+').unwrap_or(t);
    if t.is_empty() || t == "-" {
        return None;
    }
    t.parse::<i32>().ok()
}

pub fn number(n: i32) -> String {
    n.to_string()
}

// ------------------------------------------------------------------ settings store

/// %APPDATA%\aipets\settings.ini, one section per pet. Read and written under one named mutex
/// (shared with every other aipets process); changes are noticed by the file's timestamp.
pub mod store {
    use super::*;

    const MUTEX: &str = r"Local\aipets.settings";

    pub fn file_path() -> PathBuf {
        data_dir().join("settings.ini")
    }

    pub fn load() -> Ini {
        try_load().unwrap_or_default()
    }

    /// Like load, but None when the file exists and cannot be read: never defaults in place of real settings.
    pub fn try_load() -> Option<Ini> {
        locked(|| read().map(|ini| ini.unwrap_or_default())).flatten()
    }

    /// Changes keys of one section; None removes a key.
    pub fn update(section: &str, values: &[(&str, Option<&str>)]) {
        change(|ini| {
            for (k, v) in values {
                ini.set(section, k, *v);
            }
        });
    }

    /// Reads the file, lets change edit it and writes it back, all in one go under the mutex.
    pub fn change(change: impl FnOnce(&mut Ini)) {
        locked(|| {
            let mut ini = match read() {
                Some(ini) => ini.unwrap_or_default(),
                None => return,
            };
            let before = ini.format();
            change(&mut ini);
            let after = ini.format();
            if after == before {
                return;
            }
            let _ = std::fs::create_dir_all(data_dir());
            if let Err(e) = atomic_write(&file_path(), &after) {
                log(&format!("settings: {e}"));
            }
        });
    }

    pub fn stamp() -> u64 {
        win::file_stamp(&file_path())
    }

    /// Some(None): no file yet. None: the file could not be read (locked for a moment five times in a row).
    fn read() -> Option<Option<Ini>> {
        let path = file_path();
        for attempt in 1..=5 {
            match std::fs::read(&path) {
                Ok(bytes) => return Some(Some(Ini::parse(&win::decode_text(&bytes), "app"))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(None),
                Err(e) => {
                    if attempt == 5 {
                        log(&format!("settings: {e}"));
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100)); // virus scanner, backup: better late than defaults
                }
            }
        }
        None
    }

    fn locked<T>(f: impl FnOnce() -> T) -> Option<T> {
        let r = win::with_mutex(MUTEX, 3000, f);
        if r.is_none() {
            log("settings: lock timed out; operation skipped");
        }
        r
    }
}

/// Write beside the destination, then replace it in one step. A failed write keeps the old file.
pub fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let temp = PathBuf::from(format!("{}.{}.tmp", path.display(), unique_id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// 32 hex digits, different on every call (a Guid "N" in the C# version).
pub fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = win::now_ticks();
    let pid = win::current_pid() as u64;
    let mut x = t ^ (pid << 32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    format!("{:016x}{:016x}", t, x)
}

// ------------------------------------------------------------------ autostart

pub mod autostart {
    use super::*;

    fn command() -> String {
        format!("\"{}\"", exe_path())
    }

    pub fn enabled() -> bool {
        win::run_key_get(NAME).map(|v| win::eq_ignore_case(&v, &command())).unwrap_or(false)
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        let cmd = command();
        win::run_key_set(NAME, if on { Some(&cmd) } else { None })
    }

    /// The user's choice: switches autostart and remembers a "no" ([app] autostart=0 in settings.ini).
    pub fn choose(on: bool) -> Result<(), String> {
        set_enabled(on)?;
        store::update("app", &[("autostart", if on { None } else { Some("0") })]);
        Ok(())
    }

    /// Autostart is on by default: the tray switches it on when it starts (also after the folder moved)
    /// unless the user switched it off. Never for a test copy like aipets-test.exe.
    pub fn apply_default(settings: &Ini) {
        if settings.get("app", "autostart") == Some("0")
            || !win::eq_ignore_case(win::file_name(exe_path()), &format!("{NAME}.exe"))
            || enabled()
        {
            return;
        }
        match set_enabled(true) {
            Ok(()) => log("autostart switched on (default)"),
            Err(e) => log(&format!("autostart: {e}")),
        }
    }
}

// ------------------------------------------------------------------ log

static TAG: Mutex<String> = Mutex::new(String::new());

/// Which part writes: "tray", "hook claude", "install" …
pub fn set_log_tag(tag: &str) {
    if let Ok(mut t) = TAG.lock() {
        *t = tag.to_string();
    }
}

pub fn log_path() -> PathBuf {
    data_dir().join("aipets.log")
}

pub fn log(message: &str) {
    let tag = TAG.lock().map(|t| t.clone()).unwrap_or_default();
    log_as(&tag, message);
}

/// A line under another tag: each pet writes under her id, as when every pet was its own process.
pub fn log_as(tag: &str, message: &str) {
    use std::io::Write;
    let path = log_path();
    let _ = std::fs::create_dir_all(data_dir());
    if std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    let line = format!("{}  [{}] {}\r\n", win::local_timestamp(), tag, message);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Panics go to the log instead of a console nobody sees.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "panic".to_string()
        };
        let place = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        log(&format!("crash: {msg}{place}"));
    }));
}

/// "~\Desktop\…" for paths in the user's folder, shortened to 34 characters.
pub fn short_path(path: &str) -> String {
    let home = win::user_profile().to_string_lossy().into_owned();
    let mut p = path.to_string();
    if !home.is_empty() && win::starts_with_ignore_case(&p, &home) {
        p = format!("~{}", &p[home.len()..]);
    }
    let chars: Vec<char> = p.chars().collect();
    if chars.len() <= 34 {
        p
    } else {
        format!("…{}", chars[chars.len() - 33..].iter().collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_roundtrip_and_free_slots() {
        let mut ini = Ini::parse("[app]\r\nlayers=a,b\r\n\r\n[claude]\r\nx=1\r\ny=2\r\nmode=program\r\n", "app");
        assert_eq!(ini.get("CLAUDE", "X"), Some("1"));
        ini.set("claude", "x", None);
        ini.set("claude", "enabled", Some("0"));
        assert_eq!(ini.format(), "[app]\r\nlayers=a,b\r\n\r\n[claude]\r\nenabled=0\r\ny=2\r\nmode=program\r\n");
        ini.set("claude", "enabled", None);
        ini.set("claude", "y", None);
        ini.set("claude", "mode", None);
        assert_eq!(ini.format(), "[app]\r\nlayers=a,b\r\n");
        ini.set("hermes", "x", None);
        assert_eq!(ini.section_names(), vec!["app", "claude"]);
    }

    #[test]
    fn ini_parse_rules() {
        let ini = Ini::parse("; comment\nname=Claude\n# other\n =x\n[ s ]\nk = v w \n", "pet");
        assert_eq!(ini.get("pet", "name"), Some("Claude"));
        assert_eq!(ini.get("s", "k"), Some("v w"));
        assert_eq!(ini.get_int("s", "k", 7), 7);
        assert_eq!(parse_int(" -12 "), Some(-12));
        assert_eq!(parse_int("+5"), Some(5));
        assert_eq!(parse_int("1.5"), None);
    }
}
