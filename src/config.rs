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
    /// `auto` (PowerShell 7 if installed, else Windows PowerShell), or any
    /// command line, e.g. `cmd.exe` or `wsl.exe ~`.
    pub shell: String,
    /// `arrow` (path + ❯, for cmd and PowerShell), `classic` (the shell's
    /// own), or a custom cmd.exe `PROMPT` string (`$P` path, `$G` `>`, `$E`
    /// ESC, `$_` newline).
    pub prompt: String,
    /// What right-click does: `menu` (Copy, Paste, color schemes, shell,
    /// settings) or `paste` (copy a selection, else paste, like conhost).
    pub right_click: String,
    /// `block`, `bar` or `underline`.
    pub cursor_style: String,
    pub cursor_blink: bool,
    /// How solid the background is, 0.0 (clear) to 1.0 (solid).
    pub opacity: f32,
    /// What shows through a see-through background on Windows 11:
    /// `none`, `acrylic` (blurred), `mica` (wallpaper tint) or `tabbed`.
    pub backdrop: String,
    /// BEL from a program: `visual` (flash), `sound`, `both` or `none`.
    /// Unfocused windows also flash in the taskbar.
    pub bell: String,
    /// Copy to the clipboard as soon as a selection is made.
    pub copy_on_select: bool,
    /// `always`, `multiple` (only with two or more tabs) or `never`.
    pub tab_bar: String,
    /// Global hotkey that shows/hides tacoterm from anywhere, e.g.
    /// `win+\``; empty disables.
    pub summon_hotkey: String,
    /// Drop down from the top of the screen like a Quake console when
    /// summoned.
    pub quake_mode: bool,
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
    pub select_all: String,
    pub find: String,
    pub scroll_page_up: String,
    pub scroll_page_down: String,
    pub scroll_to_top: String,
    pub scroll_to_bottom: String,
    pub new_tab: String,
    pub close_pane: String,
    pub next_tab: String,
    pub previous_tab: String,
    pub split_right: String,
    pub split_down: String,
    pub focus_left: String,
    pub focus_right: String,
    pub focus_up: String,
    pub focus_down: String,
    pub resize_left: String,
    pub resize_right: String,
    pub resize_up: String,
    pub resize_down: String,
    pub zoom_in: String,
    pub zoom_out: String,
    pub zoom_reset: String,
    pub fullscreen: String,
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
            shell: "auto".to_string(),
            prompt: "arrow".to_string(),
            right_click: "menu".to_string(),
            cursor_style: "block".to_string(),
            cursor_blink: true,
            opacity: 1.0,
            backdrop: "none".to_string(),
            bell: "visual".to_string(),
            copy_on_select: false,
            tab_bar: "always".to_string(),
            summon_hotkey: "win+`".to_string(),
            quake_mode: false,
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
            select_all: "ctrl+shift+a".to_string(),
            find: "ctrl+shift+f".to_string(),
            scroll_page_up: "shift+pageup".to_string(),
            scroll_page_down: "shift+pagedown".to_string(),
            scroll_to_top: "ctrl+shift+home".to_string(),
            scroll_to_bottom: "ctrl+shift+end".to_string(),
            new_tab: "ctrl+shift+t".to_string(),
            close_pane: "ctrl+shift+w".to_string(),
            next_tab: "ctrl+tab".to_string(),
            previous_tab: "ctrl+shift+tab".to_string(),
            split_right: "alt+shift+=".to_string(),
            split_down: "alt+shift+-".to_string(),
            focus_left: "alt+left".to_string(),
            focus_right: "alt+right".to_string(),
            focus_up: "alt+up".to_string(),
            focus_down: "alt+down".to_string(),
            resize_left: "alt+shift+left".to_string(),
            resize_right: "alt+shift+right".to_string(),
            resize_up: "alt+shift+up".to_string(),
            resize_down: "alt+shift+down".to_string(),
            zoom_in: "ctrl+=".to_string(),
            zoom_out: "ctrl+-".to_string(),
            zoom_reset: "ctrl+0".to_string(),
            fullscreen: "f11".to_string(),
            next_color_scheme: "ctrl+shift+n".to_string(),
            open_settings: "ctrl+,".to_string(),
        }
    }
}

/// The `arrow` prompt for cmd.exe: blue path, green arrow, in palette
/// colors so it follows the scheme. It also reports the directory
/// (`OSC 9;9`) so new tabs and splits open where you are.
pub const ARROW_PROMPT: &str = "$E]9;9;$P$E\\$E[1;34m$P$E[0m $E[32m❯$E[0m ";

/// The first `arrow` prompt, before it reported the directory; files that
/// spelled it out mean `arrow`.
const OLD_ARROW_PROMPT: &str = "$E[1;34m$P$E[0m $E[32m❯$E[0m ";

/// The `arrow` prompt for PowerShell, installed only when the profile
/// hasn't set a prompt of its own (oh-my-posh, starship… win).
pub const POWERSHELL_ARROW_PROMPT: &str = "if ((Get-Item function:prompt).ScriptBlock.ToString() -match 'executionContext\\.SessionState\\.Path\\.CurrentLocation') { function global:prompt { $e = [char]27; \"$e]9;9;$($executionContext.SessionState.Path.CurrentLocation)$e\\$e[1;34m$($executionContext.SessionState.Path.CurrentLocation)$e[0m $e[32m$([char]0x276F)$e[0m \" } }";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Cmd,
    PowerShell,
    Other,
}

impl ShellKind {
    /// Classify a command line by its program name.
    pub fn of(command: &str) -> ShellKind {
        let command = command.trim();
        let program = match command.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or(""),
            None => command.split(' ').next().unwrap_or(""),
        };
        let name = program
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or("")
            .to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        match name {
            "cmd" => ShellKind::Cmd,
            "pwsh" | "powershell" => ShellKind::PowerShell,
            _ => ShellKind::Other,
        }
    }
}

/// Where to look for programs: `PATH`, then `fallback`.
fn find_program(name: &str, fallback: Option<PathBuf>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .or(fallback.filter(|p| p.is_file()))
}

fn system32(file: &str) -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    PathBuf::from(root).join("System32").join(file)
}

fn quote(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s
    }
}

fn has_script_args(command: &str) -> bool {
    let lower = command.to_lowercase();
    [
        "-command",
        "-c ",
        "-file",
        "-f ",
        "-encodedcommand",
        "-noexit",
    ]
    .iter()
    .any(|a| lower.contains(a))
}

/// A shell the right-click menu offers, if it's installed.
pub struct ShellChoice {
    pub label: &'static str,
    /// What goes in `shell =`.
    pub setting: &'static str,
}

pub fn available_shells() -> Vec<ShellChoice> {
    let mut out = vec![ShellChoice {
        label: "PowerShell",
        setting: "auto",
    }];
    out.push(ShellChoice {
        label: "Command Prompt",
        setting: "cmd.exe",
    });
    if system32("wsl.exe").is_file() {
        out.push(ShellChoice {
            label: "WSL (Linux)",
            setting: "wsl.exe ~",
        });
    }
    out
}

/// Encode a PowerShell script for `-EncodedCommand` (base64 of UTF-16LE),
/// which sidesteps command-line quoting entirely.
pub fn powershell_encode(script: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(TABLE[(n >> shift) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

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

/// See-through background material (Windows 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    None,
    Acrylic,
    Mica,
    Tabbed,
}

/// What a BEL does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bell {
    pub flash: bool,
    pub sound: bool,
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

    /// The command line to spawn and the environment it needs: `shell`
    /// with `auto` resolved and the `arrow` prompt wired in.
    pub fn shell_command(&self) -> (String, Vec<(&'static str, String)>) {
        let auto = self.shell.trim().is_empty() || self.shell.trim().eq_ignore_ascii_case("auto");
        let command = if auto {
            let ps = find_program("pwsh.exe", None).or_else(|| {
                find_program(
                    "powershell.exe",
                    Some(system32("WindowsPowerShell\\v1.0\\powershell.exe")),
                )
            });
            match ps {
                Some(p) => format!("{} -NoLogo", quote(&p)),
                None => quote(&system32("cmd.exe")),
            }
        } else {
            self.shell.trim().to_string()
        };
        let prompt = self.prompt.trim();
        // The old starter file spelled `arrow` out as cmd PROMPT codes.
        let arrow = prompt.eq_ignore_ascii_case("arrow")
            || prompt == ARROW_PROMPT.trim()
            || prompt == OLD_ARROW_PROMPT.trim();
        let classic = prompt.is_empty() || prompt.eq_ignore_ascii_case("classic");
        let mut env = Vec::new();
        match ShellKind::of(&command) {
            ShellKind::Cmd if arrow => env.push(("PROMPT", ARROW_PROMPT.to_string())),
            ShellKind::Cmd if !classic => env.push(("PROMPT", self.prompt.clone())),
            // Only touch a PowerShell launched without its own arguments
            // (beyond -NoLogo), so a custom -Command isn't clobbered.
            ShellKind::PowerShell if arrow && !has_script_args(&command) => {
                let cmd = format!(
                    "{command} -NoExit -EncodedCommand {}",
                    powershell_encode(POWERSHELL_ARROW_PROMPT)
                );
                return (cmd, env);
            }
            _ => {}
        }
        (command, env)
    }

    pub fn right_click_menu(&self) -> bool {
        !self.right_click.trim().eq_ignore_ascii_case("paste")
    }

    pub fn cursor_style(&self) -> CursorStyle {
        match self.cursor_style.trim().to_lowercase().as_str() {
            "bar" | "beam" | "line" => CursorStyle::Bar,
            "underline" | "underscore" => CursorStyle::Underline,
            _ => CursorStyle::Block,
        }
    }

    pub fn backdrop(&self) -> Backdrop {
        match self.backdrop.trim().to_lowercase().as_str() {
            "acrylic" | "blur" => Backdrop::Acrylic,
            "mica" => Backdrop::Mica,
            "tabbed" | "mica-alt" => Backdrop::Tabbed,
            _ => Backdrop::None,
        }
    }

    pub fn bell(&self) -> Bell {
        match self.bell.trim().to_lowercase().as_str() {
            "none" | "off" => Bell {
                flash: false,
                sound: false,
            },
            "sound" | "audible" => Bell {
                flash: false,
                sound: true,
            },
            "both" => Bell {
                flash: true,
                sound: true,
            },
            _ => Bell {
                flash: true,
                sound: false,
            },
        }
    }

    /// Everything wrong with this config that parsing alone doesn't catch.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = self.theme().1;
        if !(0.0..=1.0).contains(&self.opacity) {
            w.push(format!(
                "opacity {}: use a number from 0.0 (clear) to 1.0 (solid)",
                self.opacity
            ));
        }
        let backdrop = self.backdrop.trim().to_lowercase();
        if !matches!(
            backdrop.as_str(),
            "none" | "" | "acrylic" | "blur" | "mica" | "tabbed" | "mica-alt"
        ) {
            w.push(format!(
                "backdrop \"{}\": use none, acrylic, mica or tabbed",
                self.backdrop
            ));
        }
        let tab_bar = self.tab_bar.trim().to_lowercase();
        if !matches!(tab_bar.as_str(), "always" | "multiple" | "auto" | "never") {
            w.push(format!(
                "tab_bar \"{}\": use always, multiple or never",
                self.tab_bar
            ));
        }
        let bell = self.bell.trim().to_lowercase();
        if !matches!(
            bell.as_str(),
            "visual" | "flash" | "sound" | "audible" | "both" | "none" | "off"
        ) {
            w.push(format!(
                "bell \"{}\": use visual, sound, both or none",
                self.bell
            ));
        }
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

pub const STARTER_TOML: &str = r##"# tacoterm settings. Right-click in tacoterm for the common ones; this file
# has them all. Save it and most changes apply right away (shell and prompt
# on the next shell). Every key is optional: delete a line to get its
# default back. Ctrl+, opens this file.

# Built-in schemes: tokyo-night, catppuccin-mocha, dracula, one-half-dark,
# gruvbox-dark, nord, solarized-dark, one-half-light, campbell.
# Ctrl+Shift+N cycles through them to preview.
color_scheme = "tokyo-night"

font_family = "Cascadia Mono"
font_size = 14.0            # Ctrl+= / Ctrl+- / Ctrl+0 zoom for now
line_height = 1.25          # multiple of font_size
padding = 10                # pixels around the text
cursor_style = "block"      # block, bar or underline
cursor_blink = true
scrollback_lines = 10000

# See-through background: 1.0 is solid, 0.8 lets the desktop show through.
opacity = 1.0
# Windows 11 material behind it: "none", "acrylic" (frosted glass),
# "mica" or "tabbed". Pair acrylic with opacity = 0.6 or so.
backdrop = "none"

bell = "visual"             # visual, sound, both or none
copy_on_select = false
tab_bar = "always"          # always, multiple (2+ tabs) or never

# Press this anywhere in Windows to show or hide tacoterm. quake_mode makes
# it drop down from the top of the screen instead. (Both apply on restart.)
summon_hotkey = "win+`"
quake_mode = false

# "auto" starts PowerShell (7 if installed). Or any command line:
# "cmd.exe", "wsl.exe ~", "C:\\Program Files\\Git\\bin\\bash.exe -i -l"
shell = "auto"

# "arrow" = blue path + ❯ (cmd and PowerShell; a prompt your PowerShell
# profile sets wins), "classic" = the shell's own (PS C:\> or C:\>), or a
# cmd.exe PROMPT string ($P path, $G >, $_ new line, $E[32m colors).
prompt = "arrow"

# "menu": right-click shows Copy, Paste, tabs, panes and settings.
# "paste": right-click copies a selection or pastes, like the old console.
right_click = "menu"

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

# Mouse: drag to select, double-click a word, triple-click a line,
# Ctrl+click a link, Ctrl+wheel zooms, Shift+drag selects inside apps that
# use the mouse. An empty string unbinds a key.
[keybindings]
copy = "ctrl+shift+c"
paste = "ctrl+shift+v"
select_all = "ctrl+shift+a"
find = "ctrl+shift+f"
scroll_page_up = "shift+pageup"
scroll_page_down = "shift+pagedown"
scroll_to_top = "ctrl+shift+home"
scroll_to_bottom = "ctrl+shift+end"
new_tab = "ctrl+shift+t"
close_pane = "ctrl+shift+w"
next_tab = "ctrl+tab"
previous_tab = "ctrl+shift+tab"
split_right = "alt+shift+="
split_down = "alt+shift+-"
focus_left = "alt+left"
focus_right = "alt+right"
focus_up = "alt+up"
focus_down = "alt+down"
resize_left = "alt+shift+left"
resize_right = "alt+shift+right"
resize_up = "alt+shift+up"
resize_down = "alt+shift+down"
zoom_in = "ctrl+="
zoom_out = "ctrl+-"
zoom_reset = "ctrl+0"
fullscreen = "f11"
next_color_scheme = "ctrl+shift+n"
open_settings = "ctrl+,"
"##;

/// The first starter file pinned cmd.exe, which has no `clear`, `ls`… Move
/// files still holding that untouched line to `auto` (PowerShell).
const OLD_STARTER_SHELL: &str =
    "shell = \"C:\\\\Windows\\\\System32\\\\cmd.exe\"   # or \"pwsh.exe -NoLogo\", \"wsl.exe ~\"";

/// Ctrl+Shift+T cycled color schemes before tabs existed; it opens a tab
/// now.
const OLD_SCHEME_KEY: &str = "next_color_scheme = \"ctrl+shift+t\"";
const OLD_SCHEME_COMMENT: &str = "# Ctrl+Shift+T cycles through them to preview.";

/// Update lines older starter files wrote that mean something else now.
fn migrate(path: &std::path::Path, text: String) -> String {
    let new = text
        .replace(
            OLD_STARTER_SHELL,
            "shell = \"auto\"   # PowerShell; or \"cmd.exe\", \"wsl.exe ~\", any command line",
        )
        .replace(OLD_SCHEME_KEY, "next_color_scheme = \"ctrl+shift+n\"")
        .replace(
            OLD_SCHEME_COMMENT,
            "# Ctrl+Shift+N cycles through them to preview.",
        );
    if new != text {
        let _ = std::fs::write(path, &new);
    }
    new
}

/// Set one top-level `key = value` in the config file, keeping everything
/// else (comments included). `value` is TOML, e.g. `"\"dracula\""`.
pub fn set_top_level(key: &str, value: &str) -> std::io::Result<()> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| STARTER_TOML.to_string());
    std::fs::write(&path, with_top_level(&text, key, value))
}

fn with_top_level(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let mut out = Vec::new();
    let mut done = false;
    let mut in_table = false;
    for l in text.lines() {
        let t = l.trim_start();
        if t.starts_with('[') {
            if !done {
                out.push(line.clone());
                done = true;
            }
            in_table = true;
        }
        let is_key = !in_table
            && t.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='));
        if is_key && !done {
            out.push(line.clone());
            done = true;
        } else if !is_key {
            out.push(l.to_string());
        }
    }
    if !done {
        out.push(line);
    }
    out.join("\n") + "\n"
}

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
        Ok(text) => match parse(&migrate(&path, text)) {
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
    fn set_top_level_keeps_the_rest() {
        let text =
            "# hi\ncolor_scheme = \"nord\"  # c\nfont_size = 12.0\n[theme]\ncolor_scheme = \"x\"\n";
        let out = with_top_level(text, "color_scheme", "\"dracula\"");
        assert_eq!(
            out,
            "# hi\ncolor_scheme = \"dracula\"\nfont_size = 12.0\n[theme]\ncolor_scheme = \"x\"\n"
        );
        let out = with_top_level("[theme]\n", "shell", "\"auto\"");
        assert_eq!(out, "shell = \"auto\"\n[theme]\n");
        assert_eq!(
            parse(&with_top_level(STARTER_TOML, "padding", "3"))
                .unwrap()
                .padding,
            3
        );
    }

    #[test]
    fn old_starter_shell_migrates() {
        let old = STARTER_TOML.replace("shell = \"auto\"", OLD_STARTER_SHELL);
        assert_eq!(parse(&old).unwrap().shell, "C:\\Windows\\System32\\cmd.exe");
        let dir = std::env::temp_dir().join(format!("tacoterm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let migrated = migrate(&dir.join("config.toml"), old);
        assert_eq!(parse(&migrated).unwrap().shell, "auto");
        let old = STARTER_TOML.replace("next_color_scheme = \"ctrl+shift+n\"", OLD_SCHEME_KEY);
        let migrated = parse(&migrate(&dir.join("config.toml"), old)).unwrap();
        assert_eq!(migrated.keybindings.next_color_scheme, "ctrl+shift+n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shell_kinds_and_prompts() {
        assert_eq!(ShellKind::of("cmd.exe"), ShellKind::Cmd);
        assert_eq!(
            ShellKind::of("C:\\Windows\\System32\\cmd.exe /k"),
            ShellKind::Cmd
        );
        assert_eq!(
            ShellKind::of("\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\" -NoLogo"),
            ShellKind::PowerShell
        );
        assert_eq!(ShellKind::of("wsl.exe ~"), ShellKind::Other);

        let cmd = Config {
            shell: "cmd.exe".into(),
            ..Config::default()
        };
        assert_eq!(
            cmd.shell_command(),
            (
                "cmd.exe".to_string(),
                vec![("PROMPT", ARROW_PROMPT.to_string())]
            )
        );
        let classic = Config {
            prompt: "classic".into(),
            ..cmd.clone()
        };
        assert!(classic.shell_command().1.is_empty());

        let ps = Config {
            shell: "pwsh.exe -NoLogo".into(),
            ..Config::default()
        };
        let (line, env) = ps.shell_command();
        assert!(
            line.starts_with("pwsh.exe -NoLogo -NoExit -EncodedCommand "),
            "{line}"
        );
        assert!(env.is_empty());
        let custom = Config {
            shell: "pwsh.exe -NoExit -Command x".into(),
            ..Config::default()
        };
        assert_eq!(custom.shell_command().0, "pwsh.exe -NoExit -Command x");
    }

    #[test]
    fn powershell_encoding() {
        // [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes("dir"))
        assert_eq!(powershell_encode("dir"), "ZABpAHIA");
        assert_eq!(powershell_encode("ab"), "YQBiAA==");
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex("#0c0c0c"), Some((12, 12, 12)));
        assert_eq!(parse_hex("0c0c0c"), None);
        assert_eq!(parse_hex("#fff"), None);
    }
}
