//! deptty configuration: TOML at ~/.config/deptty/config.toml.
//! Missing file or keys fall back to defaults; unknown keys are ignored.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// terminal font point size
    pub font_size: i32,
    /// scrollback lines kept by the terminal emulator
    pub scrollback: usize,
    /// shell to launch (None -> $SHELL or /bin/bash)
    pub shell: Option<String>,
    /// font family (None -> system monospace); MUST be monospace for a sane grid
    pub font_family: Option<String>,
    /// alacritty-style key bindings: [[key_binding]] key="T" mods="Ctrl+Shift" action="new_tab"
    pub key_bindings: Vec<KeyBinding>,
    /// text cursor shape for the focused pane; unfocused panes always draw a
    /// hollow block (deepin-terminal focus hint)
    pub cursor_shape: CursorShape,
    /// colorscheme name ("breeze", ...) or path to a theme .toml; None ->
    /// default deepin palette (auto dark/light from the system window color)
    pub theme: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: 12,
            scrollback: 10_000,
            shell: None,
            font_family: None,
            key_bindings: default_key_bindings(),
            cursor_shape: CursorShape::default(),
            theme: None,
        }
    }
}

// ---- themes: ghostty-style colorscheme files ----

/// user theme dir: ~/.config/deptty/themes (XDG-aware, like config_dir)
fn themes_dir() -> std::path::PathBuf {
    config_dir().join("themes")
}

/// system theme dir (packaged .deb installs breeze.toml here)
pub const SYSTEM_THEMES_DIR: &str = "/usr/share/deptty/themes";

/// palette in ghostty theme files: either the classic 16-entry array
/// (`palette = ["#232627", ...]`) or the newer `N=COLOR` table form
/// (`palette = { 0 = "#232627", ... }` / `palette.0 = "#232627"`).
/// 16 entries only (ANSI colors); extra indexes are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Palette {
    List(Vec<String>),
    Map(std::collections::HashMap<String, String>),
}

impl Palette {
    fn get(&self, i: usize) -> Option<&str> {
        match self {
            Palette::List(v) => v.get(i).map(String::as_str),
            Palette::Map(m) => m.get(&i.to_string()).map(String::as_str),
        }
    }
}

/// colorscheme file, ghostty field conventions (kebab-case, hex colors,
/// optional palette). Unknown keys (cursor-text, selection-*, fonts, ...) are
/// accepted and ignored so stock ghostty theme files load unmodified.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Theme {
    /// file base name without .toml, or "<builtin>" for the embedded default
    pub name: String,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor_color: Option<String>,
    pub palette: Option<Palette>,
    // accepted for ghostty-file compat; deptty paints selection as a
    // translucent overlay, so these are parsed but unused
    pub selection_background: Option<String>,
    pub selection_foreground: Option<String>,
    pub cursor_text: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "".into(),
            background: None,
            foreground: None,
            cursor_color: None,
            palette: None,
            selection_background: None,
            selection_foreground: None,
            cursor_text: None,
        }
    }
}

/// `#rrggbb` or `rrggbb` -> (r, g, b); None on anything else
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// embedded default so `theme = "breeze"` works on dev builds too (the
/// packaged system dir only exists after `cargo deb`)
const BREEZE_TOML: &str = include_str!("../themes/breeze.toml");

impl Theme {
    /// resolve `name` against the user and system theme dirs (user wins), then
    /// the embedded breeze default. A name containing a path separator or
    /// ending in `.toml` is treated as a direct file path.
    pub fn load(name: &str) -> Option<Theme> {
        Self::load_from(
            name,
            &themes_dir(),
            &std::path::PathBuf::from(SYSTEM_THEMES_DIR),
        )
    }

    fn load_from(
        name: &str,
        user_dir: &std::path::Path,
        system_dir: &std::path::Path,
    ) -> Option<Theme> {
        let file = Self::find_file(name, user_dir, system_dir);
        let text = match file {
            Some(p) => std::fs::read_to_string(&p).ok()?,
            None => {
                if name == "breeze" {
                    BREEZE_TOML.to_owned()
                } else {
                    return None;
                }
            }
        };
        let mut theme: Theme = toml::from_str(&text).ok()?;
        theme.name = std::path::Path::new(name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.to_owned());
        Some(theme)
    }

    fn find_file(
        name: &str,
        user_dir: &std::path::Path,
        system_dir: &std::path::Path,
    ) -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(name);
        if name.contains(std::path::MAIN_SEPARATOR) || name.ends_with(".toml") {
            return (p.is_file()).then(|| p.to_path_buf());
        }
        let f = p.with_extension("toml");
        let user = user_dir.join(&f);
        if user.is_file() {
            return Some(user);
        }
        let sys = system_dir.join(&f);
        if sys.is_file() {
            return Some(sys);
        }
        None
    }

    pub fn bg(&self) -> Option<(u8, u8, u8)> {
        self.background.as_deref().and_then(parse_hex)
    }
    pub fn fg(&self) -> Option<(u8, u8, u8)> {
        self.foreground.as_deref().and_then(parse_hex)
    }
    pub fn cursor(&self) -> Option<(u8, u8, u8)> {
        self.cursor_color.as_deref().and_then(parse_hex)
    }
    pub fn palette16(&self) -> Option<[(u8, u8, u8); 16]> {
        let p = self.palette.as_ref()?;
        let mut out = [(0u8, 0u8, 0u8); 16];
        for i in 0..16 {
            out[i] = parse_hex(p.get(i)?)?;
        }
        Some(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Copy,
    Paste,
    NewTab,
    /// close the whole tab (default-bound to Ctrl+Shift+W)
    CloseTab,
    /// close the focused workspace (split); config key exists but is not
    /// bound by default — bind it if you want a one-key split close
    CloseWorkspace,
    NextTab,
    PrevTab,
    /// horizontal divider, panes top/bottom (konsole Split View Top/Bottom)
    SplitHorizontal,
    /// vertical divider, panes left/right (konsole Split View Left/Right)
    SplitVertical,
    /// cycle pane focus in leaf order (konsole Ctrl+Tab / Ctrl+Shift+Tab)
    NextPane,
    PrevPane,
    /// directional pane focus (deepin-terminal select_*_workspace, Alt+arrows)
    FocusPaneUp,
    FocusPaneDown,
    FocusPaneLeft,
    FocusPaneRight,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyBinding {
    /// single character ("T", ";") or a qt key name ("Left", "Tab", "F5", "Escape", ...)
    pub key: String,
    /// "+"-joined: Ctrl, Shift, Alt (e.g. "Ctrl+Shift")
    pub mods: String,
    pub action: Action,
}

fn default_key_bindings() -> Vec<KeyBinding> {
    let kb = |key: &str, mods: &str, action: Action| KeyBinding {
        key: key.into(),
        mods: mods.into(),
        action,
    };
    vec![
        kb("C", "Ctrl+Shift", Action::Copy),
        kb("V", "Ctrl+Shift", Action::Paste),
        kb("T", "Ctrl+Shift", Action::NewTab),
        kb("W", "Ctrl+Shift", Action::CloseTab),
        // konsole tab nav: Shift+Left/Right
        kb("Right", "Shift", Action::NextTab),
        kb("Left", "Shift", Action::PrevTab),
        // konsole split defaults: Ctrl+Shift+( left/right, Ctrl+Shift+) top/bottom
        kb("(", "Ctrl+Shift", Action::SplitVertical),
        kb(")", "Ctrl+Shift", Action::SplitHorizontal),
        // konsole view cycling: Ctrl+Tab next pane, Ctrl+Shift+Tab previous
        // (Qt delivers Shift+Tab as Key_Backtab); directional focus is the
        // Ctrl+Shift+arrows below, matching konsole's Focus * Terminal actions
        kb("Tab", "Ctrl", Action::NextPane),
        kb("Backtab", "Ctrl+Shift", Action::PrevPane),
        // konsole directional view focus (Focus Above/Below/Left/Right Terminal)
        kb("Up", "Ctrl+Shift", Action::FocusPaneUp),
        kb("Down", "Ctrl+Shift", Action::FocusPaneDown),
        kb("Left", "Ctrl+Shift", Action::FocusPaneLeft),
        kb("Right", "Ctrl+Shift", Action::FocusPaneRight),
    ]
}

impl KeyBinding {
    /// dtk::qt::modifier mask
    pub fn mod_mask(&self) -> i32 {
        let mut m = 0;
        for part in self.mods.split('+') {
            m |= match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => dtk::qt::modifier::CONTROL,
                "shift" => dtk::qt::modifier::SHIFT,
                "alt" => dtk::qt::modifier::ALT,
                _ => 0,
            };
        }
        m
    }
    /// qt key code: ASCII for single chars, qt::key::* for names
    pub fn key_code(&self) -> Option<i32> {
        use dtk::qt::key;
        let s = self.key.as_str();
        if let Some(c) = s.chars().next()
            && s.chars().count() == 1
        {
            return Some(c.to_ascii_uppercase() as i32);
        }
        Some(match s.to_ascii_lowercase().as_str() {
            "escape" | "esc" => key::ESCAPE,
            "tab" => key::TAB,
            "backtab" => key::BACKTAB,
            "backspace" => key::BACKSPACE,
            "return" | "enter" => key::RETURN,
            "left" => key::LEFT,
            "right" => key::RIGHT,
            "up" => key::UP,
            "down" => key::DOWN,
            "home" => key::HOME,
            "end" => key::END,
            "delete" => key::DELETE,
            "insert" => key::INSERT,
            "pageup" => key::PAGE_UP,
            "pagedown" => key::PAGE_DOWN,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shape_defaults_to_block() {
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(cfg.cursor_shape, CursorShape::Block);
        let cfg: Config = toml::from_str("cursor_shape = \"beam\"").expect("beam parses");
        assert_eq!(cfg.cursor_shape, CursorShape::Beam);
        let cfg: Config = toml::from_str("cursor_shape = \"underline\"").expect("underline parses");
        assert_eq!(cfg.cursor_shape, CursorShape::Underline);
    }

    #[test]
    fn theme_key_parses() {
        let cfg: Config = toml::from_str("theme = \"breeze\"").expect("theme parses");
        assert_eq!(cfg.theme.as_deref(), Some("breeze"));
        let cfg: Config = toml::from_str("").expect("empty parses");
        assert_eq!(cfg.theme, None);
    }

    fn temp_dirs() -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "deptty-theme-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("?")
        ));
        let user = base.join("user");
        let system = base.join("system");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        (user, system)
    }

    fn write_theme(dir: &std::path::Path, name: &str, bg: &str) {
        let t = format!("background = \"{bg}\"\nforeground = \"#fcfcfc\"\n");
        std::fs::write(dir.join(format!("{name}.toml")), t).unwrap();
    }

    #[test]
    fn theme_user_overrides_system() {
        let (user, system) = temp_dirs();
        write_theme(&system, "x", "#010101");
        write_theme(&user, "x", "#020202");
        let t = Theme::load_from("x", &user, &system).unwrap();
        assert_eq!(t.bg(), Some((2, 2, 2)));
    }

    #[test]
    fn theme_system_only_and_unknown() {
        let (user, system) = temp_dirs();
        write_theme(&system, "x", "#010101");
        let t = Theme::load_from("x", &user, &system).unwrap();
        assert_eq!(t.bg(), Some((1, 1, 1)));
        assert!(Theme::load_from("nope", &user, &system).is_none());
    }

    #[test]
    fn theme_breeze_builtin_always_resolves() {
        let (user, system) = temp_dirs();
        let t = Theme::load_from("breeze", &user, &system).unwrap();
        assert_eq!(t.bg(), Some((0x23, 0x26, 0x27))); // KDE Breeze #232627
        assert_eq!(t.fg(), Some((0xfc, 0xfc, 0xfc)));
        let p = t.palette16().unwrap();
        assert_eq!(p[1], (0xed, 0x15, 0x15)); // red
        assert_eq!(p[8], (0x7f, 0x8c, 0x8d)); // bright black
    }

    #[test]
    fn theme_palette_forms_and_hex() {
        assert_eq!(parse_hex("#23A6b7"), Some((0x23, 0xa6, 0xb7)));
        assert_eq!(parse_hex("ffffff"), Some((0xff, 0xff, 0xff)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("nope"), None);
        // array form (all 16)
        let t: Theme = toml::from_str(
            "palette = [\"#010203\", \"#040506\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\"]",
        )
        .unwrap();
        assert_eq!(
            t.palette16().map(|p| (p[0], p[1])),
            Some(((1, 2, 3), (4, 5, 6)))
        );
        // ghostty N=COLOR table form (all 16)
        let t: Theme = toml::from_str(
            "palette = { 1 = \"#040506\", 0 = \"#010203\", 2 = \"#000000\", 3 = \"#000000\", 4 = \"#000000\", 5 = \"#000000\", 6 = \"#000000\", 7 = \"#000000\", 8 = \"#000000\", 9 = \"#000000\", 10 = \"#000000\", 11 = \"#000000\", 12 = \"#000000\", 13 = \"#000000\", 14 = \"#000000\", 15 = \"#000000\" }",
        )
        .unwrap();
        assert_eq!(
            t.palette16().map(|p| (p[0], p[1])),
            Some(((1, 2, 3), (4, 5, 6)))
        );
    }

    #[test]
    fn shipped_themes_all_parse() {
        // every theme bundled for the .deb must load by name and carry a
        // full 16-entry palette (cargo test runs from the crate root)
        let dir = std::path::Path::new("themes");
        let user = std::env::temp_dir().join(format!("deptty-ship-{}", std::process::id()));
        let system = dir.to_path_buf(); // the repo themes/ dir stands in for /usr/share
        let mut found = 0;
        for entry in std::fs::read_dir(dir).expect("themes dir") {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            let t = Theme::load_from(&name, &user, &system).expect("theme loads");
            assert!(t.bg().is_some(), "{name}: missing background");
            assert!(t.fg().is_some(), "{name}: missing foreground");
            assert!(
                t.palette16().is_some(),
                "{name}: palette must have 16 valid hex"
            );
            found += 1;
        }
        assert!(
            found >= 8,
            "expected at least 8 shipped themes, got {found}"
        );
    }

    #[test]
    fn pane_navigation_bindings_present() {
        let cfg = Config::default();
        let has = |key: &str, mods: &str, action: Action| {
            cfg.key_bindings
                .iter()
                .any(|b| b.key == key && b.mods == mods && b.action == action)
        };
        assert!(has("Tab", "Ctrl", Action::NextPane));
        assert!(has("Backtab", "Ctrl+Shift", Action::PrevPane));
        // konsole directional view focus
        assert!(has("Up", "Ctrl+Shift", Action::FocusPaneUp));
        assert!(has("Down", "Ctrl+Shift", Action::FocusPaneDown));
        assert!(has("Left", "Ctrl+Shift", Action::FocusPaneLeft));
        assert!(has("Right", "Ctrl+Shift", Action::FocusPaneRight));
        // konsole tab nav is plain Shift+arrows; the Ctrl+Shift+arrow slot
        // belongs to pane focus, so no Alt defaults anywhere
        assert!(has("Left", "Shift", Action::PrevTab));
        assert!(has("Right", "Shift", Action::NextTab));
        assert!(cfg.key_bindings.iter().all(|b| b.mods != "Alt"));
    }
}

/// XDG_CONFIG_HOME already IS the config dir; only append .config under $HOME
fn config_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("~"))
                .join(".config")
        })
        .join("deptty")
}

/// remembered session state (konsole's konsolestaterc equivalent): kept out of
/// config.toml so user comments/ordering survive
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// last window size in pixels; applied on startup (deepin-terminal window_width/height)
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
}

impl State {
    pub fn load() -> Self {
        let path = config_dir().join("state.toml");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let dir = config_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        if let Ok(text) = toml::to_string(self) {
            let _ = std::fs::write(dir.join("state.toml"), text);
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("deptty: bad config {}: {e}, using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}
