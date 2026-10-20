//! The command palette (every action, color scheme and shell, filtered as
//! you type) and quick select (labels on everything worth copying on
//! screen; typing a label copies it).

use std::collections::HashMap;

use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use tacoterm::config::{self, Keybindings};
use tacoterm::input::Action;
use tacoterm::layout::{Direction, PaneId};
use tacoterm::renderer::{Hint, PaletteView};
use tacoterm::schemes;

use super::{show_warning, toml_string, App};

enum Command {
    Run(Action),
    Scheme(&'static str),
    /// Open a tab running this `shell =` setting.
    Shell(String),
    RenameTab,
    ResetTabName,
    CloseTab,
    EditSettingsFile,
}

struct Entry {
    label: String,
    hint: String,
    command: Command,
}

#[derive(PartialEq)]
enum Mode {
    Commands,
    /// Typing a new name for the active tab.
    Rename,
}

pub(super) struct Palette {
    mode: Mode,
    query: String,
    selected: usize,
    entries: Vec<Entry>,
    /// Indices into `entries` that match the query, best first.
    shown: Vec<usize>,
}

/// `ctrl+shift+p` as `Ctrl+Shift+P`.
fn pretty_combo(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| {
            let mut c = part.trim().chars();
            match c.next() {
                Some(first) => first.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn actions(keys: &Keybindings) -> Vec<(&'static str, &str, Action)> {
    use Direction::*;
    vec![
        ("New tab", &keys.new_tab, Action::NewTab),
        ("Split right", &keys.split_right, Action::SplitRight),
        ("Split down", &keys.split_down, Action::SplitDown),
        ("Close pane", &keys.close_pane, Action::ClosePane),
        ("Next tab", &keys.next_tab, Action::NextTab),
        ("Previous tab", &keys.previous_tab, Action::PreviousTab),
        ("Find", &keys.find, Action::Find),
        ("Quick select", &keys.quick_select, Action::QuickSelect),
        ("Copy", &keys.copy, Action::Copy),
        ("Paste", &keys.paste, Action::Paste),
        ("Select all", &keys.select_all, Action::SelectAll),
        (
            "Jump to previous prompt",
            &keys.previous_prompt,
            Action::PreviousPrompt,
        ),
        ("Jump to next prompt", &keys.next_prompt, Action::NextPrompt),
        (
            "Scroll up a page",
            &keys.scroll_page_up,
            Action::ScrollPageUp,
        ),
        (
            "Scroll down a page",
            &keys.scroll_page_down,
            Action::ScrollPageDown,
        ),
        ("Scroll to top", &keys.scroll_to_top, Action::ScrollToTop),
        (
            "Scroll to bottom",
            &keys.scroll_to_bottom,
            Action::ScrollToBottom,
        ),
        ("Focus pane left", &keys.focus_left, Action::Focus(Left)),
        ("Focus pane right", &keys.focus_right, Action::Focus(Right)),
        ("Focus pane above", &keys.focus_up, Action::Focus(Up)),
        ("Focus pane below", &keys.focus_down, Action::Focus(Down)),
        ("Grow pane left", &keys.resize_left, Action::Resize(Left)),
        ("Grow pane right", &keys.resize_right, Action::Resize(Right)),
        ("Grow pane up", &keys.resize_up, Action::Resize(Up)),
        ("Grow pane down", &keys.resize_down, Action::Resize(Down)),
        ("Zoom in", &keys.zoom_in, Action::ZoomIn),
        ("Zoom out", &keys.zoom_out, Action::ZoomOut),
        ("Reset zoom", &keys.zoom_reset, Action::ZoomReset),
        ("Toggle full screen", &keys.fullscreen, Action::Fullscreen),
        (
            "Preview next color scheme",
            &keys.next_color_scheme,
            Action::NextColorScheme,
        ),
        ("Open settings", &keys.open_settings, Action::OpenSettings),
    ]
}

/// How well `query` fuzzy-matches `text`: None unless every query
/// character appears in order. Runs of consecutive characters and matches
/// at word starts score higher, and so do shorter texts.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0;
    let mut at = 0;
    let mut last: Option<usize> = None;
    for q in query.chars().flat_map(char::to_lowercase) {
        if q == ' ' {
            continue;
        }
        let i = at + text[at..].iter().position(|&c| c == q)?;
        score += 1;
        if last.is_some_and(|l| l + 1 == i) {
            score += 5;
        }
        if i == 0 || !text[i - 1].is_alphanumeric() {
            score += 8;
        }
        last = Some(i);
        at = i + 1;
    }
    Some(score * 100 - text.len() as i32)
}

impl Palette {
    fn filter(&mut self) {
        if self.mode == Mode::Rename {
            self.shown.clear();
            return;
        }
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy_score(&self.query, &e.label).map(|s| (s, i)))
            .collect();
        if !self.query.is_empty() {
            scored.sort_by_key(|&(s, i)| (-s, i));
        }
        self.shown = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }

    pub(super) fn view(&self) -> PaletteView<'_> {
        PaletteView {
            placeholder: match self.mode {
                Mode::Commands => "Type a command",
                Mode::Rename => "New tab name (empty goes back to the shell's title)",
            },
            query: &self.query,
            items: self
                .shown
                .iter()
                .map(|&i| {
                    (
                        self.entries[i].label.as_str(),
                        self.entries[i].hint.as_str(),
                    )
                })
                .collect(),
            selected: self.selected,
        }
    }
}

/// Quick select: the targets on screen and the label keys typed so far.
pub(super) struct QuickSelect {
    pane: PaneId,
    /// (label, text) per target, in [`Self::hints`] order.
    targets: Vec<(String, String)>,
    all: Vec<Hint>,
    typed: String,
    /// The targets whose label starts with `typed`, labelled with the rest.
    pub(super) hints: Vec<Hint>,
}

/// Keys the labels are made of, easiest first.
const LABEL_KEYS: &str = "asdfjklghqwertyuiopzxcvbnm";

/// Labels for `n` targets: single keys while they last, otherwise pairs
/// (so no label is a prefix of another).
pub fn quick_labels(n: usize) -> Vec<String> {
    let keys: Vec<char> = LABEL_KEYS.chars().collect();
    if n <= keys.len() {
        keys[..n].iter().map(|c| c.to_string()).collect()
    } else {
        keys.iter()
            .flat_map(|&a| keys.iter().map(move |&b| format!("{a}{b}")))
            .take(n)
            .collect()
    }
}

impl QuickSelect {
    fn update(&mut self) {
        self.hints = self
            .all
            .iter()
            .filter_map(|h| {
                let rest = h.label.strip_prefix(self.typed.as_str())?;
                Some(Hint {
                    label: rest.to_string(),
                    ..h.clone()
                })
            })
            .collect();
    }

    pub(super) fn pane(&self) -> PaneId {
        self.pane
    }
}

impl App {
    pub(super) fn open_palette(&mut self) {
        if self.palette.take().is_some() {
            return;
        }
        self.close_search();
        self.quick = None;
        let mut entries: Vec<Entry> = actions(&self.config.keybindings)
            .into_iter()
            .map(|(label, key, action)| Entry {
                label: label.to_string(),
                hint: pretty_combo(key),
                command: Command::Run(action),
            })
            .collect();
        let extra = [
            ("Rename tab", Command::RenameTab),
            ("Reset tab name", Command::ResetTabName),
            ("Close tab", Command::CloseTab),
            ("Edit config.toml", Command::EditSettingsFile),
        ];
        for (label, command) in extra {
            entries.push(Entry {
                label: label.to_string(),
                hint: String::new(),
                command,
            });
        }
        for shell in config::available_shells() {
            entries.push(Entry {
                label: format!("New tab: {}", shell.label),
                hint: String::new(),
                command: Command::Shell(shell.setting.to_string()),
            });
        }
        for s in schemes::SCHEMES {
            entries.push(Entry {
                label: format!("Color scheme: {}", s.name),
                hint: if s.name == self.scheme {
                    "current".to_string()
                } else {
                    String::new()
                },
                command: Command::Scheme(s.name),
            });
        }
        let mut palette = Palette {
            mode: Mode::Commands,
            query: String::new(),
            selected: 0,
            entries,
            shown: Vec::new(),
        };
        palette.filter();
        self.palette = Some(palette);
    }

    /// Put `query` in the open palette's filter box.
    pub(super) fn palette_query(&mut self, query: &str) {
        if let Some(p) = &mut self.palette {
            p.query = query.to_string();
            p.filter();
        }
        self.redraw();
    }

    /// Open the palette straight into naming the active tab.
    pub(super) fn rename_tab(&mut self) {
        self.close_search();
        self.quick = None;
        let query = self
            .tabs
            .get(self.active)
            .and_then(|t| t.name.clone())
            .unwrap_or_default();
        self.palette = Some(Palette {
            mode: Mode::Rename,
            query,
            selected: 0,
            entries: Vec::new(),
            shown: Vec::new(),
        });
        self.redraw();
    }

    /// A key while the palette is open. Everything goes to it.
    pub(super) fn palette_key(&mut self, event: &KeyEvent) {
        let Some(p) = &mut self.palette else { return };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.palette = None,
            Key::Named(NamedKey::Enter) => {
                let Some(p) = self.palette.take() else { return };
                if p.mode == Mode::Rename {
                    let name = p.query.trim();
                    if let Some(t) = self.tabs.get_mut(self.active) {
                        t.name = (!name.is_empty()).then(|| name.to_string());
                    }
                    self.update_title();
                } else if let Some(&i) = p.shown.get(p.selected) {
                    let mut entries = p.entries;
                    let command = entries.swap_remove(i).command;
                    self.run_command(command);
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                let n = p.shown.len().max(1);
                p.selected = (p.selected + n - 1) % n;
            }
            Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::Tab) => {
                let n = p.shown.len().max(1);
                p.selected = (p.selected + 1) % n;
            }
            Key::Named(NamedKey::PageUp) => p.selected = p.selected.saturating_sub(10),
            Key::Named(NamedKey::PageDown) => {
                p.selected = (p.selected + 10).min(p.shown.len().saturating_sub(1));
            }
            Key::Named(NamedKey::Backspace) => {
                if self.mods.control_key() {
                    p.query.clear();
                } else {
                    p.query.pop();
                }
                p.filter();
            }
            _ => {
                if let Some(t) = event
                    .text
                    .as_deref()
                    .filter(|t| !t.chars().any(char::is_control))
                {
                    if !self.mods.control_key() || self.mods.alt_key() {
                        p.query.push_str(t);
                        p.filter();
                    }
                }
            }
        }
        self.redraw();
    }

    fn run_command(&mut self, command: Command) {
        match command {
            Command::Run(action) => self.run_action(action),
            Command::Scheme(name) => {
                self.scheme = name.to_string();
                self.config.color_scheme = name.to_string();
                self.apply_theme(&self.config.theme().0);
                self.save_setting("color_scheme", &toml_string(name));
            }
            Command::Shell(setting) => {
                if let Err(e) = self.new_tab_with(&setting) {
                    show_warning(e);
                }
            }
            Command::RenameTab => self.rename_tab(),
            Command::ResetTabName => {
                if let Some(t) = self.tabs.get_mut(self.active) {
                    t.name = None;
                }
                self.update_title();
            }
            Command::CloseTab => self.close_tab(self.active),
            Command::EditSettingsFile => self.edit_settings_file(),
        }
        self.redraw();
    }

    pub(super) fn start_quick_select(&mut self) {
        if self.quick.take().is_some() {
            return;
        }
        let Some(id) = self.focused_id() else { return };
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let found = pane.grid.quick_targets();
        if found.is_empty() {
            self.ring();
            return;
        }
        // The same text gets the same label; the newest (lowest) targets
        // get the easiest keys.
        let mut texts: Vec<&str> = Vec::new();
        for (_, text) in found.iter().rev() {
            if !texts.contains(&text.as_str()) {
                texts.push(text);
            }
        }
        let labels: HashMap<&str, String> = texts
            .iter()
            .copied()
            .zip(quick_labels(texts.len()))
            .collect();
        let mut targets = Vec::new();
        let mut all = Vec::new();
        for (m, text) in &found {
            let label = labels[text.as_str()].clone();
            all.push(Hint {
                start: m.start,
                end: m.end,
                label: label.clone(),
            });
            targets.push((label, text.clone()));
        }
        self.close_search();
        self.palette = None;
        let mut quick = QuickSelect {
            pane: id,
            targets,
            all,
            typed: String::new(),
            hints: Vec::new(),
        };
        quick.update();
        self.quick = Some(quick);
    }

    /// A key during quick select. A label copies its text; typed in
    /// capitals it also pastes it into the shell.
    pub(super) fn quick_key(&mut self, event: &KeyEvent) {
        let Some(q) = &mut self.quick else { return };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.quick = None,
            Key::Named(NamedKey::Backspace) => {
                q.typed.pop();
                q.update();
            }
            _ => {
                let Some(c) = event.text.as_deref().and_then(|t| t.chars().next()) else {
                    return;
                };
                if !c.is_alphabetic() {
                    return;
                }
                let paste = c.is_uppercase();
                q.typed.extend(c.to_lowercase());
                let picked = q
                    .targets
                    .iter()
                    .find(|(label, _)| *label == q.typed)
                    .map(|(_, text)| text.clone());
                if let Some(text) = picked {
                    let pane = q.pane;
                    self.quick = None;
                    if let Some(cb) = self.clipboard() {
                        let _ = cb.set_text(text.clone());
                    }
                    if paste {
                        if let Some(p) = self.panes.get_mut(&pane) {
                            let _ = p.pty.write_input(text.as_bytes());
                        }
                    }
                } else {
                    q.update();
                    if q.hints.is_empty() {
                        // No label starts that way: start over.
                        q.typed.clear();
                        q.update();
                    }
                }
            }
        }
        self.redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(fuzzy_score("spr", "Split right").is_some());
        assert!(fuzzy_score("xyz", "Split right").is_none());
        let word = fuzzy_score("nt", "New tab").unwrap();
        let mid = fuzzy_score("nt", "Find next").unwrap();
        assert!(word > mid);
        assert_eq!(pretty_combo("ctrl+shift+p"), "Ctrl+Shift+P");
    }

    #[test]
    fn labels_never_prefix_each_other() {
        assert_eq!(quick_labels(3), ["a", "s", "d"]);
        let many = quick_labels(30);
        assert_eq!(many.len(), 30);
        assert!(many.iter().all(|l| l.len() == 2));
    }
}
