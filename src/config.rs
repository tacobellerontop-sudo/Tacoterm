//! TOML config: font, theme, keybindings, default shell.
//!
//! Written to %APPDATA%\tacoterm\config.toml on first run (like slowshell:
//! the starter is written once and never rewritten, so your file is yours).
//! Every key is optional; anything missing takes its default.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font_family: String,
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Blank space around the grid, in pixels.
    pub padding: u32,
    pub scrollback_lines: usize,
    /// Command line to run, e.g. `pwsh.exe -NoLogo` or `wsl.exe ~`.
    pub shell: String,
    pub theme: Theme,
    pub keybindings: Keybindings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection: String,
    /// 16 ANSI colors as #rrggbb: 0-7 normal, 8-15 bright.
    pub palette: Vec<String>,
}

/// Each value is a key combo like `ctrl+shift+c`; an empty string unbinds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Keybindings {
    pub copy: String,
    pub paste: String,
    pub scroll_page_up: String,
    pub scroll_page_down: String,
    pub scroll_to_top: String,
    pub scroll_to_bottom: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_family: "Cascadia Mono".to_string(),
            font_size: 14.0,
            line_height: 1.2,
            padding: 4,
            scrollback_lines: 10_000,
            shell: "C:\\Windows\\System32\\cmd.exe".to_string(),
            theme: Theme::default(),
            keybindings: Keybindings::default(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background: "#0c0c0c".to_string(),
            foreground: "#cccccc".to_string(),
            cursor: "#cccccc".to_string(),
            selection: "#264f78".to_string(),
            palette: [
                "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd",
                "#cccccc", "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e",
                "#61d6d6", "#f2f2f2",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

impl Default for Keybindings {
    fn default() -> Self {
        Keybindings {
            copy: "ctrl+shift+c".to_string(),
            paste: "ctrl+shift+v".to_string(),
            scroll_page_up: "shift+pageup".to_string(),
            scroll_page_down: "shift+pagedown".to_string(),
            scroll_to_top: "ctrl+shift+home".to_string(),
            scroll_to_bottom: "ctrl+shift+end".to_string(),
        }
    }
}

pub fn config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("tacoterm").join("config.toml")
}

pub const STARTER_TOML: &str = r##"# tacoterm config — edit and it applies on next launch.
# Every key is optional: delete a line to get the default back.

font_family = "Cascadia Mono"
font_size = 14.0
line_height = 1.2          # multiple of font_size
padding = 4                # pixels around the grid
scrollback_lines = 10000
shell = "C:\\Windows\\System32\\cmd.exe"  # or "pwsh.exe -NoLogo", "wsl.exe ~"

[theme]
background = "#0c0c0c"
foreground = "#cccccc"
cursor = "#cccccc"
selection = "#264f78"
palette = [
  "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00",
  "#0037da", "#881798", "#3a96dd", "#cccccc",
  "#767676", "#e74856", "#16c60c", "#f9f1a5",
  "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
]

# Mouse: drag to select, right-click copies a selection or pastes.
[keybindings]
copy = "ctrl+shift+c"
paste = "ctrl+shift+v"
scroll_page_up = "shift+pageup"
scroll_page_down = "shift+pagedown"
scroll_to_top = "ctrl+shift+home"
scroll_to_bottom = "ctrl+shift+end"
"##;

/// Parse config text. Errors carry a human-readable message.
pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Load the config, writing the starter file on first run.
/// A broken config falls back to defaults and returns a warning for the
/// app to show — a bad config never blanks the terminal.
pub fn load() -> (Config, Option<String>) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                Config::default(),
                Some(format!(
                    "bad config {}: {}; using defaults",
                    path.display(),
                    e.trim_end()
                )),
            ),
        },
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, STARTER_TOML);
            (Config::default(), None)
        }
    }
}

/// Parse #rrggbb into (r, g, b).
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_matches_defaults() {
        let cfg = parse(STARTER_TOML).expect("starter config parses");
        let def = Config::default();
        assert_eq!(cfg.font_family, def.font_family);
        assert_eq!(cfg.shell, def.shell);
        assert_eq!(cfg.theme.palette, def.theme.palette);
        assert_eq!(cfg.keybindings.copy, def.keybindings.copy);
    }

    #[test]
    fn partial_config_fills_defaults() {
        let cfg = parse("font_size = 18.0\n[theme]\nbackground = \"#101010\"\n").unwrap();
        assert_eq!(cfg.font_size, 18.0);
        assert_eq!(cfg.theme.background, "#101010");
        assert_eq!(cfg.theme.foreground, "#cccccc");
        assert_eq!(cfg.keybindings.paste, "ctrl+shift+v");
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex("#0c0c0c"), Some((12, 12, 12)));
        assert_eq!(parse_hex("0c0c0c"), None);
        assert_eq!(parse_hex("#fff"), None);
    }
}
