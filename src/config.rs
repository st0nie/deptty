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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: 12,
            scrollback: 10_000,
            shell: None,
            font_family: None,
            key_bindings: default_key_bindings(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Copy,
    Paste,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    /// horizontal divider, panes top/bottom (konsole Split View Top/Bottom)
    SplitHorizontal,
    /// vertical divider, panes left/right (konsole Split View Left/Right)
    SplitVertical,
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
        kb("Right", "Ctrl+Shift", Action::NextTab),
        kb("Left", "Ctrl+Shift", Action::PrevTab),
        // konsole split defaults: Ctrl+Shift+( left/right, Ctrl+Shift+) top/bottom
        kb("(", "Ctrl+Shift", Action::SplitVertical),
        kb(")", "Ctrl+Shift", Action::SplitHorizontal),
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
            && s.chars().count() == 1 {
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
