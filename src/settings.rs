//! The settings page's model: which settings it shows, how each one is
//! edited (pick from a list, slide a number, flip a switch), and saving the
//! result back to config.toml.
//!
//! Edits go to a draft [`Config`] that the app applies live, so the whole
//! window (and the page's preview) shows the result before it is saved.

use crate::config::{self, Config};
use crate::schemes;

/// How a setting is edited.
#[derive(Clone, Debug)]
pub enum Kind {
    /// One of these (value, label) pairs.
    Choice(Vec<(String, String)>),
    /// A number from `min` to `max` in steps of `step`, shown with `unit`.
    Number {
        min: f32,
        max: f32,
        step: f32,
        /// `%` shows the value ×100.
        unit: &'static str,
    },
    Toggle,
    /// Typed text. ‹ › step through `presets`, if there are any.
    Text {
        presets: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub struct Field {
    /// The config.toml key.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
}

fn choice(pairs: &[(&str, &str)]) -> Kind {
    Kind::Choice(
        pairs
            .iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect(),
    )
}

/// "tokyo-night" -> "Tokyo Night".
pub fn pretty(name: &str) -> String {
    name.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The settings page's state: the draft being edited and the selection.
pub struct Settings {
    pub fields: Vec<Field>,
    pub draft: Config,
    original: Config,
    pub selected: usize,
}

impl Settings {
    /// `fonts` are the installed monospace families; `shells` the
    /// (setting, label) pairs the shell picker offers.
    pub fn new(config: &Config, mut fonts: Vec<String>, shells: Vec<(String, String)>) -> Settings {
        if !fonts.iter().any(|f| f == &config.font_family) {
            fonts.insert(0, config.font_family.clone());
        }
        let mut shell_choices: Vec<(String, String)> = shells;
        if !shell_choices.iter().any(|(v, _)| v == &config.shell) {
            shell_choices.push((config.shell.clone(), config.shell.clone()));
        }
        let fields = vec![
            Field {
                key: "color_scheme",
                label: "Color scheme",
                help: "Colors for text, background, cursor and selection.",
                kind: Kind::Choice(
                    schemes::SCHEMES
                        .iter()
                        .map(|s| (s.name.to_string(), pretty(s.name)))
                        .collect(),
                ),
            },
            Field {
                key: "opacity",
                label: "Opacity",
                help: "How solid the background is. Lower lets the desktop show through.",
                kind: Kind::Number {
                    min: 0.2,
                    max: 1.0,
                    step: 0.05,
                    unit: "%",
                },
            },
            Field {
                key: "backdrop",
                label: "Background effect",
                help: "Windows 11 material behind a see-through background.",
                kind: choice(&[
                    ("none", "None"),
                    ("acrylic", "Acrylic (frosted)"),
                    ("mica", "Mica"),
                    ("tabbed", "Mica Alt"),
                ]),
            },
            Field {
                key: "font_family",
                label: "Font",
                help: "Monospace fonts installed on this PC.",
                kind: Kind::Choice(fonts.into_iter().map(|f| (f.clone(), f)).collect()),
            },
            Field {
                key: "font_size",
                label: "Font size",
                help: "Ctrl+= and Ctrl+- zoom without changing this.",
                kind: Kind::Number {
                    min: 8.0,
                    max: 36.0,
                    step: 1.0,
                    unit: "pt",
                },
            },
            Field {
                key: "line_height",
                label: "Line height",
                help: "Space between lines, as a multiple of the font size.",
                kind: Kind::Number {
                    min: 1.0,
                    max: 2.0,
                    step: 0.05,
                    unit: "×",
                },
            },
            Field {
                key: "padding",
                label: "Padding",
                help: "Blank space around the text, in pixels.",
                kind: Kind::Number {
                    min: 0.0,
                    max: 40.0,
                    step: 2.0,
                    unit: "px",
                },
            },
            Field {
                key: "cursor_style",
                label: "Cursor",
                help: "Programs like vim can still change it while they run.",
                kind: choice(&[
                    ("block", "Block █"),
                    ("bar", "Bar │"),
                    ("underline", "Underline ▁"),
                ]),
            },
            Field {
                key: "cursor_blink",
                label: "Blinking cursor",
                help: "Blink the cursor while tacoterm is in front.",
                kind: Kind::Toggle,
            },
            Field {
                key: "shell",
                label: "Shell",
                help: "What new tabs run. Open tabs keep their shell.",
                kind: Kind::Choice(shell_choices),
            },
            Field {
                key: "prompt",
                label: "Prompt",
                help: "Styled: the name, folder and symbol below. Classic: the shell's own PS C:\\>. New tabs.",
                kind: choice(&[("arrow", "Styled"), ("classic", "Classic")]),
            },
            Field {
                key: "prompt_name",
                label: "Prompt name",
                help: "Type a name to show first in the prompt; Backspace deletes. Empty for none.",
                kind: Kind::Text {
                    presets: Vec::new(),
                },
            },
            Field {
                key: "prompt_path",
                label: "Prompt folder",
                help: "How much of the current folder the prompt shows (cmd always shows all of it).",
                kind: choice(&[
                    ("full", "Full path"),
                    ("folder", "Folder name"),
                    ("none", "Hidden"),
                ]),
            },
            Field {
                key: "prompt_symbol",
                label: "Prompt symbol",
                help: "Type your own, or ‹ › for ideas. Empty for none.",
                kind: Kind::Text {
                    presets: ["❯", ">", "$", "#", "%", "λ", "➜", "→", "»", "▶", ""]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                },
            },
            Field {
                key: "tab_bar",
                label: "Tab bar",
                help: "With the custom title bar the tab strip always shows.",
                kind: choice(&[
                    ("always", "Always"),
                    ("multiple", "With 2+ tabs"),
                    ("never", "Never"),
                ]),
            },
            Field {
                key: "title_bar",
                label: "Title bar",
                help: "Custom draws it see-through into the tab strip. Applies on restart.",
                kind: choice(&[("custom", "Custom"), ("system", "Windows")]),
            },
            Field {
                key: "bell",
                label: "Bell",
                help: "What happens when a program rings the bell.",
                kind: choice(&[
                    ("visual", "Flash"),
                    ("sound", "Sound"),
                    ("both", "Flash and sound"),
                    ("none", "Nothing"),
                ]),
            },
            Field {
                key: "right_click",
                label: "Right-click",
                help: "Menu, or copy a selection / paste like the old console.",
                kind: choice(&[("menu", "Menu"), ("paste", "Copy / paste")]),
            },
            Field {
                key: "copy_on_select",
                label: "Copy on select",
                help: "Copy text to the clipboard as soon as you select it.",
                kind: Kind::Toggle,
            },
            Field {
                key: "scrollback_lines",
                label: "Scrollback",
                help: "Lines of history kept per pane.",
                kind: choice(&[
                    ("1000", "1,000 lines"),
                    ("5000", "5,000 lines"),
                    ("10000", "10,000 lines"),
                    ("50000", "50,000 lines"),
                    ("100000", "100,000 lines"),
                ]),
            },
            Field {
                key: "quake_mode",
                label: "Quake mode",
                help: "Win+` drops tacoterm down from the top of the screen. On restart.",
                kind: Kind::Toggle,
            },
        ];
        Settings {
            fields,
            draft: config.clone(),
            original: config.clone(),
            selected: 0,
        }
    }

    /// The draft's value for `key`, as config.toml would spell it (without
    /// quotes).
    fn raw(&self, key: &str) -> String {
        let c = &self.draft;
        match key {
            "color_scheme" => c.color_scheme.clone(),
            "opacity" => format!("{:.2}", c.opacity),
            "backdrop" => c.backdrop.trim().to_lowercase(),
            "font_family" => c.font_family.clone(),
            "font_size" => format!("{}", c.font_size),
            "line_height" => format!("{:.2}", c.line_height),
            "padding" => c.padding.to_string(),
            "cursor_style" => c.cursor_style.trim().to_lowercase(),
            "cursor_blink" => c.cursor_blink.to_string(),
            "shell" => c.shell.clone(),
            "prompt" => c.prompt.trim().to_lowercase(),
            "prompt_name" => c.prompt_name.clone(),
            "prompt_path" => c.prompt_path.trim().to_lowercase(),
            "prompt_symbol" => c.prompt_symbol.clone(),
            "tab_bar" => c.tab_bar.trim().to_lowercase(),
            "title_bar" => c.title_bar.trim().to_lowercase(),
            "bell" => c.bell.trim().to_lowercase(),
            "right_click" => c.right_click.trim().to_lowercase(),
            "copy_on_select" => c.copy_on_select.to_string(),
            "scrollback_lines" => c.scrollback_lines.to_string(),
            "quake_mode" => c.quake_mode.to_string(),
            _ => String::new(),
        }
    }

    fn set_raw(&mut self, key: &str, v: &str) {
        let c = &mut self.draft;
        let num = || v.parse::<f32>().ok();
        match key {
            "color_scheme" => c.color_scheme = v.to_string(),
            "opacity" => c.opacity = num().unwrap_or(c.opacity),
            "backdrop" => c.backdrop = v.to_string(),
            "font_family" => c.font_family = v.to_string(),
            "font_size" => c.font_size = num().unwrap_or(c.font_size),
            "line_height" => c.line_height = num().unwrap_or(c.line_height),
            "padding" => c.padding = num().map_or(c.padding, |n| n as u32),
            "cursor_style" => c.cursor_style = v.to_string(),
            "cursor_blink" => c.cursor_blink = v == "true",
            "shell" => c.shell = v.to_string(),
            "prompt" => c.prompt = v.to_string(),
            "prompt_name" => c.prompt_name = v.to_string(),
            "prompt_path" => c.prompt_path = v.to_string(),
            "prompt_symbol" => c.prompt_symbol = v.to_string(),
            "tab_bar" => c.tab_bar = v.to_string(),
            "title_bar" => c.title_bar = v.to_string(),
            "bell" => c.bell = v.to_string(),
            "right_click" => c.right_click = v.to_string(),
            "copy_on_select" => c.copy_on_select = v == "true",
            "scrollback_lines" => c.scrollback_lines = v.parse().unwrap_or(c.scrollback_lines),
            "quake_mode" => c.quake_mode = v == "true",
            _ => {}
        }
    }

    /// The value as the page shows it.
    pub fn value_text(&self, i: usize) -> String {
        let f = &self.fields[i];
        let raw = self.raw(f.key);
        match &f.kind {
            Kind::Choice(options) => options
                .iter()
                .find(|(v, _)| v.eq_ignore_ascii_case(&raw))
                .map_or(raw, |(_, l)| l.clone()),
            Kind::Number { unit: "%", .. } => {
                format!("{:.0}%", raw.parse::<f32>().unwrap_or(0.0) * 100.0)
            }
            Kind::Number { unit, step, .. } => {
                let n = raw.parse::<f32>().unwrap_or(0.0);
                if step.fract() == 0.0 {
                    format!("{n:.0} {unit}")
                } else {
                    format!("{n:.2}{unit}")
                }
            }
            Kind::Toggle => if raw == "true" { "On" } else { "Off" }.to_string(),
            Kind::Text { .. } => raw,
        }
    }

    pub fn is_text(&self, i: usize) -> bool {
        matches!(self.fields[i].kind, Kind::Text { .. })
    }

    /// Whether ‹ › step a text setting through suggestions.
    pub fn has_presets(&self, i: usize) -> bool {
        matches!(&self.fields[i].kind, Kind::Text { presets } if !presets.is_empty())
    }

    /// Type into a text setting.
    pub fn type_text(&mut self, i: usize, text: &str) {
        if !self.is_text(i) {
            return;
        }
        let key = self.fields[i].key;
        let typed: String = text.chars().filter(|c| !c.is_control()).collect();
        let mut v = self.raw(key);
        // Long enough for any name; a prompt shouldn't eat the line.
        if v.chars().count() + typed.chars().count() <= 24 {
            v.push_str(&typed);
            self.set_raw(key, &v);
        }
    }

    /// Delete the last character of a text setting.
    pub fn backspace(&mut self, i: usize) {
        if !self.is_text(i) {
            return;
        }
        let key = self.fields[i].key;
        let mut v = self.raw(key);
        v.pop();
        self.set_raw(key, &v);
    }

    pub fn toggle_state(&self, i: usize) -> bool {
        self.raw(self.fields[i].key) == "true"
    }

    /// Where a number sits between its min and max, 0..=1.
    pub fn fraction(&self, i: usize) -> Option<f32> {
        let f = &self.fields[i];
        let Kind::Number { min, max, .. } = f.kind else {
            return None;
        };
        let n = self.raw(f.key).parse::<f32>().ok()?;
        Some(((n - min) / (max - min)).clamp(0.0, 1.0))
    }

    /// Step a setting: the next/previous choice, a number step, or a flip.
    pub fn adjust(&mut self, i: usize, delta: i32) {
        let f = self.fields[i].clone();
        let raw = self.raw(f.key);
        match &f.kind {
            Kind::Choice(options) => {
                let n = options.len() as i32;
                let at = options
                    .iter()
                    .position(|(v, _)| v.eq_ignore_ascii_case(&raw))
                    .map_or(if delta > 0 { -1 } else { 0 }, |p| p as i32);
                let next = (at + delta).rem_euclid(n) as usize;
                let v = options[next].0.clone();
                self.set_raw(f.key, &v);
            }
            Kind::Number { min, max, step, .. } => {
                let n = raw.parse::<f32>().unwrap_or(*min);
                let v = snap(n + *step * delta as f32, *min, *max, *step);
                self.set_raw(f.key, &v.to_string());
            }
            Kind::Toggle => {
                let v = if raw == "true" { "false" } else { "true" };
                self.set_raw(f.key, v);
            }
            Kind::Text { presets } if !presets.is_empty() => {
                let n = presets.len() as i32;
                let at = presets
                    .iter()
                    .position(|v| v == &raw)
                    .map_or(if delta > 0 { -1 } else { 0 }, |p| p as i32);
                let v = presets[(at + delta).rem_euclid(n) as usize].clone();
                self.set_raw(f.key, &v);
            }
            Kind::Text { .. } => {}
        }
    }

    /// Set a number from a slider position, 0..=1.
    pub fn set_fraction(&mut self, i: usize, t: f32) {
        let f = self.fields[i].clone();
        if let Kind::Number { min, max, step, .. } = f.kind {
            let v = snap(min + t.clamp(0.0, 1.0) * (max - min), min, max, step);
            self.set_raw(f.key, &v.to_string());
        }
    }

    /// The settings that differ from what was loaded, as TOML values.
    pub fn changes(&self) -> Vec<(&'static str, String)> {
        let before = Settings {
            fields: Vec::new(),
            draft: self.original.clone(),
            original: self.original.clone(),
            selected: 0,
        };
        self.fields
            .iter()
            .filter(|f| self.raw(f.key) != before.raw(f.key))
            .map(|f| {
                let raw = self.raw(f.key);
                let value = match (&f.kind, f.key) {
                    (Kind::Toggle, _) | (Kind::Number { .. }, _) | (_, "scrollback_lines") => raw,
                    _ => toml::Value::String(raw).to_string(),
                };
                (f.key, value)
            })
            .collect()
    }

    pub fn dirty(&self) -> bool {
        !self.changes().is_empty()
    }

    /// The config as it was when the page opened, to put back on cancel.
    pub fn original(&self) -> &Config {
        &self.original
    }

    /// Write the changes to config.toml.
    pub fn save(&mut self) -> std::io::Result<()> {
        let changes = self.changes();
        if !changes.is_empty() {
            config::set_top_levels(&changes)?;
        }
        self.original = self.draft.clone();
        Ok(())
    }
}

/// Round to the step grid and clamp.
fn snap(v: f32, min: f32, max: f32, step: f32) -> f32 {
    let v = min + ((v - min) / step).round() * step;
    // Keep floats tidy: 0.8500001 -> 0.85.
    ((v.clamp(min, max)) * 1000.0).round() / 1000.0
}

/// A colorful sample for the preview: prompts as `config` draws them, a
/// listing, a diff, text styles and the palette.
pub fn preview_text(config: &Config) -> String {
    const DIR: &str = "C:\\Users\\you\\project";
    let prompt = if config.prompt.trim().eq_ignore_ascii_case("arrow") {
        config.prompt_parts().sample(DIR)
    } else {
        format!("PS {DIR}> ")
    };
    [
        &format!("{prompt}ls\r\n"),
        "\x1b[34msrc\x1b[0m   \x1b[34mtests\x1b[0m   Cargo.toml   \x1b[32mbuild.ps1\x1b[0m   README.md\r\n",
        &format!("{prompt}git diff --stat\r\n"),
        " src/app.rs | 12 \x1b[32m++++++++\x1b[31m----\x1b[0m\r\n",
        "\x1b[33mwarning\x1b[0m: unused variable \x1b[1m`x`\x1b[0m  \x1b[31merror\x1b[0m: oh no\r\n",
        "\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[9mstrike\x1b[0m \x1b[2mdim\x1b[0m \x1b[7minverse\x1b[0m\r\n",
        "┌──────────┬──────────┐\r\n",
        "│ box      │ drawing  │  ░▒▓█\r\n",
        "└──────────┴──────────┘\r\n",
        "\x1b[40m  \x1b[41m  \x1b[42m  \x1b[43m  \x1b[44m  \x1b[45m  \x1b[46m  \x1b[47m  \x1b[0m\r\n",
        "\x1b[100m  \x1b[101m  \x1b[102m  \x1b[103m  \x1b[104m  \x1b[105m  \x1b[106m  \x1b[107m  \x1b[0m\r\n",
        &prompt,
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Settings {
        Settings::new(
            &Config::default(),
            vec!["Cascadia Mono".into(), "Consolas".into()],
            vec![
                ("auto".into(), "PowerShell".into()),
                ("cmd.exe".into(), "Command Prompt".into()),
            ],
        )
    }

    fn index(s: &Settings, key: &str) -> usize {
        s.fields.iter().position(|f| f.key == key).unwrap()
    }

    #[test]
    fn choices_cycle_both_ways() {
        let mut s = page();
        let i = index(&s, "color_scheme");
        assert_eq!(s.value_text(i), "Tokyo Night");
        s.adjust(i, 1);
        assert_eq!(s.draft.color_scheme, schemes::SCHEMES[1].name);
        s.adjust(i, -2);
        assert_eq!(s.draft.color_scheme, schemes::SCHEMES.last().unwrap().name);
    }

    #[test]
    fn numbers_step_snap_and_clamp() {
        let mut s = page();
        let i = index(&s, "opacity");
        assert_eq!(s.value_text(i), "100%");
        s.adjust(i, 1);
        assert_eq!(s.draft.opacity, 1.0, "clamped at the top");
        s.adjust(i, -3);
        assert_eq!(s.draft.opacity, 0.85);
        assert_eq!(s.value_text(i), "85%");
        s.set_fraction(i, 0.0);
        assert_eq!(s.draft.opacity, 0.2);
        let f = index(&s, "font_size");
        s.adjust(f, 2);
        assert_eq!(s.value_text(f), "16 pt");
        assert_eq!(s.fraction(f), Some(8.0 / 28.0));
    }

    #[test]
    fn toggles_and_changes() {
        let mut s = page();
        assert!(!s.dirty());
        let b = index(&s, "cursor_blink");
        s.adjust(b, 1);
        assert!(!s.draft.cursor_blink);
        s.adjust(index(&s, "opacity"), -2);
        s.adjust(index(&s, "backdrop"), 1);
        s.adjust(index(&s, "scrollback_lines"), 1);
        let changes = s.changes();
        assert!(changes.contains(&("cursor_blink", "false".into())));
        assert!(changes.contains(&("opacity", "0.90".into())));
        assert!(changes.contains(&("backdrop", "\"acrylic\"".into())));
        assert!(changes.contains(&("scrollback_lines", "50000".into())));
        // What save writes parses back to the draft.
        let mut text = config::STARTER_TOML.to_string();
        for (k, v) in &changes {
            text = config::with_top_level(&text, k, v);
        }
        let back = config::parse(&text).unwrap();
        assert!(!back.cursor_blink);
        assert_eq!(back.opacity, 0.9);
        assert_eq!(back.backdrop, "acrylic");
        assert_eq!(back.scrollback_lines, 50000);
    }

    #[test]
    fn prompt_text_types_and_cycles() {
        let mut s = page();
        let name = index(&s, "prompt_name");
        s.type_text(name, "LX");
        s.type_text(name, "\u{8}!");
        assert_eq!(s.draft.prompt_name, "LX!");
        s.backspace(name);
        assert_eq!(s.value_text(name), "LX");
        s.adjust(name, 1);
        assert_eq!(s.draft.prompt_name, "LX", "no presets: arrows do nothing");
        let sym = index(&s, "prompt_symbol");
        s.adjust(sym, 1);
        assert_eq!(s.draft.prompt_symbol, ">");
        s.adjust(sym, -2);
        assert_eq!(s.draft.prompt_symbol, "", "the last preset is none");
        assert!(s.changes().contains(&("prompt_name", "\"LX\"".to_string())));
        assert!(preview_text(&s.draft).starts_with("\x1b[1;35mLX\x1b[0m \x1b[1;34mC:"));
    }

    #[test]
    fn unknown_current_values_are_kept_as_choices() {
        let c = Config {
            font_family: "Iosevka".into(),
            shell: "nu.exe".into(),
            ..Config::default()
        };
        let s = Settings::new(&c, vec!["Consolas".into()], Vec::new());
        assert_eq!(s.value_text(index(&s, "font_family")), "Iosevka");
        assert_eq!(s.value_text(index(&s, "shell")), "nu.exe");
    }
}
