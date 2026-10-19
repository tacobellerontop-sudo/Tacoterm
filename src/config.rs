//! TOML config: font, color scheme, cursor, prompt, keybindings, shell.
//!
//! Written to %APPDATA%\tacoterm\config.toml on first run (like slowshell:
//! the starter is written once and never rewritten, so your file is yours).
//! Every key is optional; anything missing takes its default. The app
//! watches the file and applies most changes as soon as it is saved.

use serde::Deserialize;
use std::path::PathBuf;

use crate::schemes;

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
    /// cmd.exe prompt, in `PROMPT` syntax (`$P` path, `$G` `>`, `$E` ESC,
    /// `$_` newline). Empty keeps cmd's own. Ignored by other shells.
    pub prompt: String,
    /// `block`, `bar` or `underline`.
    pub cursor_style: String,
    /// One of the built-in schemes in [`crate::schemes`].
    pub color_scheme: String,
    /// Per-color overrides on top of the scheme.
    pub theme: ThemeOverrides,
    pub keybindings: Keybindings,
}

/// Each color is optional: unset ones come from `color_scheme`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeOverrides {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    /// Up to 16 ANSI colors as #rrggbb: 0-7 normal, 8-15 bright.
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
    pub next_color_scheme: String,
    pub open_settings: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_family: "Cascadia Mono".to_string(),
            font_size: 14.0,
            line_height: 1.25,
            padding: 10,
            scrollback_lines: 10_000,
            shell: "C:\\Windows\\System32\\cmd.exe".to_string(),
            prompt: DEFAULT_PROMPT.to_string(),
            cursor_style: "block".to_string(),
            color_scheme: "tokyo-night".to_string(),
            theme: ThemeOverrides::default(),
            keybindings: Keybindings::default(),
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
            next_color_scheme: "ctrl+shift+t".to_string(),
            open_settings: "ctrl+,".to_string(),
        }
    }
}

/// Blue path, green arrow; uses palette colors so it follows the scheme.
pub const DEFAULT_PROMPT: &str = "$E[1;34m$P$E[0m $E[32m❯$E[0m ";

pub type Rgb = (u8, u8, u8);

/// The colors to draw with: the scheme plus overrides, all parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub palette: [Rgb; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

impl Config {
    /// Resolve `color_scheme` + `[theme]` overrides. Problems (unknown
    /// scheme, bad colors) fall back to the scheme's color and are reported.
    pub fn theme(&self) -> (Theme, Vec<String>) {
        let mut warnings = Vec::new();
        let scheme = schemes::find(&self.color_scheme).unwrap_or_else(|| {
            warnings.push(format!(
                "unknown color_scheme \"{}\"; built-in schemes: {}",
                self.color_scheme,
                schemes::names()
            ));
            &schemes::SCHEMES[0]
        });
        let mut pick = |key: &str, over: Option<&String>, base: &str| -> Rgb {
            if let Some(s) = over {
                match parse_hex(s) {
                    Some(c) => return c,
                    None => warnings.push(format!("theme.{key}: \"{s}\" is not a #rrggbb color")),
                }
            }
            parse_hex(base).unwrap_or((0, 0, 0))
        };
        let background = pick(
            "background",
            self.theme.background.as_ref(),
            scheme.background,
        );
        let foreground = pick(
            "foreground",
            self.theme.foreground.as_ref(),
            scheme.foreground,
        );
        let cursor = pick("cursor", self.theme.cursor.as_ref(), scheme.cursor);
        let selection = pick("selection", self.theme.selection.as_ref(), scheme.selection);
        let mut palette = [(0, 0, 0); 16];
        for (i, slot) in palette.iter_mut().enumerate() {
            *slot = pick("palette", self.theme.palette.get(i), scheme.palette[i]);
        }
        (
            Theme {
                background,
                foreground,
                cursor,
                selection,
                palette,
            },
            warnings,
        )
    }

    pub fn cursor_style(&self) -> CursorStyle {
        match self.cursor_style.trim().to_lowercase().as_str() {
            "bar" | "beam" | "line" => CursorStyle::Bar,
            "underline" | "underscore" => CursorStyle::Underline,
            _ => CursorStyle::Block,
        }
    }

    /// Everything wrong with this config that parsing alone doesn't catch.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = self.theme().1;
        let style = self.cursor_style.trim().to_lowercase();
        if !matches!(
            style.as_str(),
            "block" | "bar" | "beam" | "line" | "underline" | "underscore"
        ) {
            w.push(format!(
                "cursor_style \"{}\": use block, bar or underline",
                self.cursor_style
            ));
        }
        w
    }
}

pub fn config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("tacoterm").join("config.toml")
}

pub const STARTER_TOML: &str = r##"# tacoterm settings. Save this file and most changes apply right away;
# shell and prompt apply to the next window. Every key is optional: delete
# a line to get its default back. Ctrl+, opens this file.

# Built-in schemes: tokyo-night, catppuccin-mocha, dracula, one-half-dark,
# gruvbox-dark, nord, solarized-dark, one-half-light, campbell.
# Ctrl+Shift+T cycles through them to preview.
color_scheme = "tokyo-night"

font_family = "Cascadia Mono"
font_size = 14.0
line_height = 1.25          # multiple of font_size
padding = 10                # pixels around the text
cursor_style = "block"      # block, bar or underline
scrollback_lines = 10000

shell = "C:\\Windows\\System32\\cmd.exe"   # or "pwsh.exe -NoLogo", "wsl.exe ~"

# cmd.exe's prompt. $P path, $G >, $_ new line, $E escape (for colors:
# $E[32m green … $E[0m reset). "" keeps cmd's own C:\> prompt.
# PowerShell and WSL set their prompt in their own profile.
prompt = "$E[1;34m$P$E[0m $E[32m❯$E[0m "

# Override single colors of the scheme (all optional):
[theme]
# background = "#1a1b26"
# foreground = "#c0caf5"
# cursor = "#c0caf5"
# selection = "#33467c"
# palette = ["#15161e", "#f7768e", "#9ece6a", "#e0af68",
#            "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
#            "#414868", "#f7768e", "#9ece6a", "#e0af68",
#            "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5"]

# Mouse: drag to select, right-click copies a selection or pastes.
[keybindings]
copy = "ctrl+shift+c"
paste = "ctrl+shift+v"
scroll_page_up = "shift+pageup"
scroll_page_down = "shift+pagedown"
scroll_to_top = "ctrl+shift+home"
scroll_to_bottom = "ctrl+shift+end"
next_color_scheme = "ctrl+shift+t"
open_settings = "ctrl+,"
"##;

/// Parse config text. Errors carry a human-readable message.
pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Load the config, writing the starter file on first run.
/// A broken config falls back to defaults and returns warnings for the
/// app to show — a bad config never blanks the terminal.
pub fn load() -> (Config, Vec<String>) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(cfg) => {
                let w = cfg.warnings();
                (cfg, w)
            }
            Err(e) => (
                Config::default(),
                vec![format!(
                    "bad config {}: {}; using defaults",
                    path.display(),
                    e.trim_end()
                )],
            ),
        },
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, STARTER_TOML);
            (Config::default(), Vec::new())
        }
    }
}

/// Parse #rrggbb into (r, g, b).
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().strip_prefix('#')?;
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
        assert_eq!(cfg.prompt, def.prompt);
        assert_eq!(cfg.color_scheme, def.color_scheme);
        assert_eq!(cfg.padding, def.padding);
        assert_eq!(cfg.keybindings.open_settings, def.keybindings.open_settings);
        assert!(cfg.warnings().is_empty());
    }

    #[test]
    fn scheme_with_overrides() {
        let cfg = parse(
            "color_scheme = \"Dracula\"\n[theme]\nbackground = \"#101010\"\npalette = [\"#010203\"]\n",
        )
        .unwrap();
        let (theme, warnings) = cfg.theme();
        assert!(warnings.is_empty());
        assert_eq!(theme.background, (0x10, 0x10, 0x10));
        assert_eq!(theme.foreground, (0xf8, 0xf8, 0xf2));
        assert_eq!(theme.palette[0], (1, 2, 3));
        assert_eq!(theme.palette[1], (0xff, 0x55, 0x55));
    }

    #[test]
    fn bad_values_warn_and_fall_back() {
        let cfg =
            parse("color_scheme = \"neon\"\ncursor_style = \"blob\"\n[theme]\ncursor = \"red\"\n")
                .unwrap();
        let w = cfg.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert_eq!(
            cfg.theme().0.background,
            parse_hex(schemes::SCHEMES[0].background).unwrap()
        );
        assert_eq!(cfg.cursor_style(), CursorStyle::Block);
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex("#0c0c0c"), Some((12, 12, 12)));
        assert_eq!(parse_hex("0c0c0c"), None);
        assert_eq!(parse_hex("#fff"), None);
    }
}
