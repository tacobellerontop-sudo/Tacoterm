//! TOML config: font, theme, shell.
//!
//! Written to %APPDATA%\tacoterm\config.toml on first run (like slowshell:
//! the starter is written once and never rewritten, so your file is yours).

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_font")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default = "default_scrollback")]
    pub scrollback_lines: usize,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default)]
    pub theme: Theme,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    #[serde(default = "default_bg")]
    pub background: String,
    #[serde(default = "default_fg")]
    pub foreground: String,
    /// 16 ANSI colors as #rrggbb: 0-7 normal, 8-15 bright.
    #[serde(default = "default_palette")]
    pub palette: Vec<String>,
}

fn default_font() -> String {
    "Cascadia Mono".to_string()
}
fn default_font_size() -> f32 {
    14.0
}
fn default_scrollback() -> usize {
    10_000
}
fn default_shell() -> String {
    "C:\\Windows\\System32\\cmd.exe".to_string()
}
fn default_bg() -> String {
    "#0c0c0c".to_string()
}
fn default_fg() -> String {
    "#cccccc".to_string()
}
fn default_palette() -> Vec<String> {
    [
        "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd",
        "#cccccc", "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e",
        "#61d6d6", "#f2f2f2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_family: default_font(),
            font_size: default_font_size(),
            scrollback_lines: default_scrollback(),
            shell: default_shell(),
            theme: Theme::default(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background: default_bg(),
            foreground: default_fg(),
            palette: default_palette(),
        }
    }
}

fn config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("tacoterm").join("config.toml")
}

fn starter_toml() -> String {
    r#"# tacoterm config — edit and it applies on next launch.
font_family = "Cascadia Mono"
font_size = 14.0
scrollback_lines = 10000
shell = "C:\\Windows\\System32\\cmd.exe"  # or powershell.exe, wsl.exe

[theme]
background = "#0c0c0c"
foreground = "#cccccc"
palette = [
  "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00",
  "#0037da", "#881798", "#3a96dd", "#cccccc",
  "#767676", "#e74856", "#16c60c", "#f9f1a5",
  "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
]
"#
    .to_string()
}

/// Load the config, writing the starter file on first run.
/// A broken config prints what is wrong and falls back to defaults —
/// a bad config never blanks the terminal.
pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("tacoterm: bad config {}: {e}; using defaults", path.display());
            Config::default()
        }),
        Err(_) => {
            let cfg = Config::default();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, starter_toml());
            cfg
        }
    }
}

/// Parse #rrggbb into (r, g, b). Used by the renderer for theme colors.
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}
