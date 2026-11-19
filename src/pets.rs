//! One pet = one folder pets\<id>\ with pet.ini (defaults) and sprites\ (atlas.png, atlas.txt, icon.ico).
//! Adding a folder adds a pet; no code changes needed.

use crate::app::{self, Ini};
use crate::win;

#[derive(Clone, Debug, Default)]
pub struct PetInfo {
    pub id: String,
    pub dir: String,
    pub name: String,
    pub open_text: String,
    pub order: i32,
    pub home: i32,
    pub program: String,
    pub find: String,
    pub args: String,
    pub bypass_args: String,
    pub shell: String,
    /// the pet's website, opened in the default browser in website mode
    pub url: String,
    /// app mode: app ids and exe paths, ';'-separated (see DesktopApp::find)
    pub desktop_app: String,
    /// app mode: the program with these arguments opens the app ("codex app"), without a window
    pub app_command: String,
    /// app mode without an installed app: run the program with these arguments in the terminal
    pub app_fallback: String,
    /// "program", "app" or "website": what a click opens unless the settings say otherwise
    pub mode: String,
    /// "claude", "hermes", "codex", "cursor" or "": which hook source the pet shows
    pub status: String,
}

impl PetInfo {
    pub fn sprites_dir(&self) -> String {
        win::combine(&self.dir, "sprites")
    }

    pub fn icon_path(&self) -> String {
        win::combine(&self.sprites_dir(), "icon.ico")
    }

    pub fn has_sprites(&self) -> bool {
        let s = self.sprites_dir();
        win::file_exists(&win::combine(&s, "atlas.png")) && win::file_exists(&win::combine(&s, "atlas.txt"))
    }

    /// The optional bypass preset only applies to the CLI named in this pet's defaults.
    pub fn bypass_arguments(&self, program: &str) -> String {
        let expected = program_identity(&self.program);
        if !expected.is_empty() && win::eq_ignore_case(&expected, &program_identity(program)) {
            self.bypass_args.clone()
        } else {
            String::new()
        }
    }

    pub fn discover() -> Vec<PetInfo> {
        let mut pets = Vec::new();
        let dir = app::pets_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else { return pets };
        let mut dirs: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        for d in dirs {
            let ini = win::combine(&d, "pet.ini");
            if !win::file_exists(&ini) {
                continue;
            }
            match win::read_text(&ini) {
                Ok(text) => pets.push(Self::load(&d, &Ini::parse(&text, "pet"))),
                Err(e) => app::log(&format!("pet.ini unreadable in {d}: {e}")),
            }
        }
        pets.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
        pets
    }

    pub fn by_id(id: &str) -> Option<PetInfo> {
        Self::discover().into_iter().find(|p| win::eq_ignore_case(&p.id, id))
    }

    fn load(dir: &str, ini: &Ini) -> PetInfo {
        let get = |k: &str| ini.get("pet", k).map(str::to_string);
        let id = win::file_name(dir).to_lowercase();
        let name = get("name").unwrap_or_else(|| id.clone());
        let program = get("program").unwrap_or_default();
        let url = get("url").unwrap_or_default();
        // without mode=, a pet that only has a url is a website pet
        let mode = checked_mode(&get("mode").unwrap_or_else(|| {
            if program.is_empty() && !url.is_empty() { "website".into() } else { "program".into() }
        }));
        PetInfo {
            open_text: get("open").unwrap_or_else(|| format!("Open {name}")),
            order: ini.get_int("pet", "order", 100),
            home: ini.get_int("pet", "home", 12),
            find: get("find").unwrap_or_default(),
            args: get("args").unwrap_or_default(),
            bypass_args: get("bypassargs").unwrap_or_default(),
            shell: get("shell").unwrap_or_else(|| "direct".into()),
            desktop_app: get("app").unwrap_or_default(),
            app_command: get("appcommand").unwrap_or_default(),
            app_fallback: get("appfallback").unwrap_or_default(),
            status: get("status").unwrap_or_default(),
            id,
            dir: dir.to_string(),
            name,
            program,
            url,
            mode,
        }
    }
}

fn program_identity(program: &str) -> String {
    let mut name = program.trim().to_string();
    if name.len() >= 2
        && ((name.starts_with('"') && name.ends_with('"')) || (name.starts_with('\'') && name.ends_with('\'')))
    {
        name = name[1..name.len() - 1].to_string();
    }
    let name = name.replace('/', "\\");
    let name = name.rsplit('\\').next().unwrap_or("").to_string();
    for ext in [".exe", ".cmd", ".bat"] {
        if win::ends_with_ignore_case(&name, ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name
}

/// "website", "app" or "program" (anything unknown).
pub fn checked_mode(mode: &str) -> String {
    if mode == "website" || mode == "app" { mode.to_string() } else { "program".to_string() }
}

/// The tallest figure (Grok) at 1:1: no pet is smaller, so no sprite loses pixels;
/// old settings counted sizes in steps of this (1×, 2× …).
pub const HEIGHT_UNIT: i32 = 162;
pub const MIN_HEIGHT: i32 = HEIGHT_UNIT;

/// A pet's effective settings: the pet.ini defaults overlaid with its section in settings.ini.
#[derive(Clone, Debug, Default)]
pub struct PetSettings {
    /// the pet's own choice ([id] enabled)
    pub enabled: bool,
    /// "Hide all pets" ([app] hidden=1); enabled stays as it was
    pub all_hidden: bool,
    /// height of all pets in screen pixels ([app] height); 0 = from the display DPI
    pub height: i32,
    /// this pet's own height ([id] height); 0 = the one of all pets
    pub own_height: i32,
    pub has_position: bool,
    /// bottom-centre of the pet in screen pixels
    pub x: i32,
    pub y: i32,
    pub work_dir: String,
    pub program: String,
    pub args: String,
    pub shell: String,
    pub url: String,
    pub desktop_app: String,
    pub mode: String,
}

impl PetSettings {
    pub fn from(pet: &PetInfo, ini: &Ini) -> PetSettings {
        let id = pet.id.as_str();
        let get = |k: &str| ini.get(id, k).map(str::to_string);
        let mut work_dir = get("workdir").unwrap_or_default();
        if work_dir.is_empty() || !win::dir_exists(&work_dir) {
            work_dir = win::user_profile().to_string_lossy().into_owned();
        }
        PetSettings {
            enabled: ini.get(id, "enabled") != Some("0"),
            all_hidden: hides_all(ini),
            height: height_of(ini, "app"),
            own_height: height_of(ini, id),
            has_position: ini.get(id, "x").is_some() && ini.get(id, "y").is_some(),
            x: ini.get_int(id, "x", 0),
            y: ini.get_int(id, "y", 0),
            work_dir,
            program: get("program").unwrap_or_else(|| pet.program.clone()),
            args: get("args").unwrap_or_else(|| pet.args.clone()),
            shell: get("shell").unwrap_or_else(|| pet.shell.clone()),
            url: get("url").unwrap_or_else(|| pet.url.clone()),
            desktop_app: get("app").unwrap_or_else(|| pet.desktop_app.clone()),
            mode: checked_mode(&get("mode").unwrap_or_else(|| pet.mode.clone())),
        }
    }

    /// The pet is on the desktop: shown herself, and not all pets hidden.
    pub fn shown(&self) -> bool {
        self.enabled && !self.all_hidden
    }

    pub fn opens_program(&self) -> bool {
        self.mode == "program"
    }

    pub fn opens_app(&self) -> bool {
        self.mode == "app"
    }

    pub fn opens_website(&self) -> bool {
        self.mode == "website"
    }

    pub fn use_mode(&mut self, mode: &str) {
        self.mode = checked_mode(mode);
    }

    /// The height of all pets without their own: the setting or the display default.
    pub fn shared_height(&self) -> i32 {
        if self.height > 0 { self.height } else { default_height() }
    }

    /// The height this pet asks for; the pet keeps it within the screen she stands on.
    pub fn pet_height(&self) -> i32 {
        if self.own_height > 0 { self.own_height } else { self.shared_height() }
    }
}

/// All pets are hidden at once; each pet's own "Show" comes back when this is off again.
pub fn hides_all(ini: &Ini) -> bool {
    ini.get("app", "hidden") == Some("1")
}

/// A height key: 0 if unset or not a number, otherwise at least MIN_HEIGHT.
fn height_of(ini: &Ini, section: &str) -> i32 {
    let px = ini.get_int(section, "height", 0);
    if px <= 0 { 0 } else { px.max(MIN_HEIGHT) }
}

/// Without a setting: 324 px on a 96-dpi display, more on scaled ones.
pub fn default_height() -> i32 {
    ((2.0 * win::system_dpi() as f64 / 96.0).round() as i32).max(1) * HEIGHT_UNIT
}

/// The height for all pets. It replaces every pet's own height: they all snap to it.
pub fn share_height(ini: &mut Ini, px: i32) {
    for section in ini.section_names() {
        ini.set(&section, "height", None);
    }
    ini.set("app", "height", Some(&px.to_string()));
}

/// "324 px"
pub fn height_text(px: i32) -> String {
    format!("{px} px")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_only_for_the_default_cli() {
        let pet = PetInfo { program: "claude".into(), bypass_args: "--x".into(), ..Default::default() };
        assert_eq!(pet.bypass_arguments("Claude.EXE"), "--x");
        assert_eq!(pet.bypass_arguments("\"C:\\a b\\claude.cmd\""), "--x");
        assert_eq!(pet.bypass_arguments("C:/tools/claude"), "--x");
        assert_eq!(pet.bypass_arguments("codex"), "");
        let none = PetInfo { program: "".into(), bypass_args: "--x".into(), ..Default::default() };
        assert_eq!(none.bypass_arguments(""), "");
    }

    #[test]
    fn heights_and_modes() {
        let ini = Ini::parse("[app]\nheight=100\nhidden=1\n[claude]\nheight=abc\nmode=app\n", "app");
        let pet = PetInfo { id: "claude".into(), mode: "website".into(), ..Default::default() };
        let s = PetSettings::from(&pet, &ini);
        assert_eq!(s.height, MIN_HEIGHT);
        assert_eq!(s.own_height, 0);
        assert!(s.all_hidden && s.enabled && !s.shown());
        assert!(s.opens_app());
        assert_eq!(checked_mode("weird"), "program");
        let mut ini = ini;
        share_height(&mut ini, 400);
        assert_eq!(ini.get("app", "height"), Some("400"));
    }
}
