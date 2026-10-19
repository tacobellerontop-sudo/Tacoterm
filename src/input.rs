//! Keyboard → VT byte sequences, and keybinding matching.
//!
//! Special keys become CSI/SS3 sequences (with modifier parameters);
//! printable keys become their text, with Ctrl/Alt applied.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Modifier parameter for CSI sequences: 1 + shift(1) + alt(2) + ctrl(4).
fn mod_param(mods: ModifiersState) -> u8 {
    1 + (mods.shift_key() as u8) + ((mods.alt_key() as u8) << 1) + ((mods.control_key() as u8) << 2)
}

/// Cursor keys: `ESC [ X` (or `ESC O X` in application cursor mode), or
/// `ESC [ 1 ; {m} X` with modifiers.
fn cursor_key(code: u8, mods: ModifiersState, app_cursor: bool) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        vec![0x1b, if app_cursor { b'O' } else { b'[' }, code]
    } else {
        format!("\x1b[1;{m}{}", code as char).into_bytes()
    }
}

/// `ESC [ {n} ~`, or `ESC [ {n} ; {m} ~` (Insert, Delete, PgUp, PgDn, F5+).
fn tilde(code: &str, mods: ModifiersState) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{m}~").into_bytes()
    }
}

/// SS3 (`ESC O X`) for F1-F4, `ESC [ 1 ; {m} X` with modifiers.
fn ss3(code: u8, mods: ModifiersState) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        vec![0x1b, b'O', code]
    } else {
        format!("\x1b[1;{m}{}", code as char).into_bytes()
    }
}

fn alt_prefix(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// Translate a key press into the bytes the PTY should receive.
/// Returns None for keys the terminal ignores (pure modifiers, etc.).
/// Key repeat is honored: holding a key repeats it, like a real terminal.
pub fn key_to_bytes(event: &KeyEvent, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }
    encode_key(&event.logical_key, event.text.as_deref(), mods, app_cursor)
}

/// The testable core of [`key_to_bytes`]: `key` is the logical key and
/// `text` the text it produced, if any.
pub fn encode_key(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();

    let seq = match key {
        Key::Named(named) => match named {
            NamedKey::Enter => alt_prefix(alt, b"\r".to_vec()),
            NamedKey::Tab if mods.shift_key() => b"\x1b[Z".to_vec(),
            NamedKey::Tab => b"\t".to_vec(),
            NamedKey::Space if ctrl => alt_prefix(alt, vec![0]),
            NamedKey::Space => alt_prefix(alt, b" ".to_vec()),
            NamedKey::Backspace if ctrl => alt_prefix(alt, vec![0x08]),
            NamedKey::Backspace => alt_prefix(alt, vec![0x7f]),
            NamedKey::Escape => vec![0x1b],
            NamedKey::ArrowUp => cursor_key(b'A', mods, app_cursor),
            NamedKey::ArrowDown => cursor_key(b'B', mods, app_cursor),
            NamedKey::ArrowRight => cursor_key(b'C', mods, app_cursor),
            NamedKey::ArrowLeft => cursor_key(b'D', mods, app_cursor),
            NamedKey::Home => cursor_key(b'H', mods, app_cursor),
            NamedKey::End => cursor_key(b'F', mods, app_cursor),
            NamedKey::Insert => tilde("2", mods),
            NamedKey::Delete => tilde("3", mods),
            NamedKey::PageUp => tilde("5", mods),
            NamedKey::PageDown => tilde("6", mods),
            NamedKey::F1 => ss3(b'P', mods),
            NamedKey::F2 => ss3(b'Q', mods),
            NamedKey::F3 => ss3(b'R', mods),
            NamedKey::F4 => ss3(b'S', mods),
            NamedKey::F5 => tilde("15", mods),
            NamedKey::F6 => tilde("17", mods),
            NamedKey::F7 => tilde("18", mods),
            NamedKey::F8 => tilde("19", mods),
            NamedKey::F9 => tilde("20", mods),
            NamedKey::F10 => tilde("21", mods),
            NamedKey::F11 => tilde("23", mods),
            NamedKey::F12 => tilde("24", mods),
            _ => return None,
        },
        Key::Character(s) => {
            let typed = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
            // Windows reports AltGr as Ctrl+Alt; when that produced real
            // text (`@` on a German layout), type the text.
            if ctrl && alt {
                if let Some(t) = typed {
                    return Some(t.as_bytes().to_vec());
                }
            }
            // Ctrl+key -> C0 control code (Ctrl+C = 0x03, etc.). Keys with
            // no control code fall through and send their text.
            if ctrl {
                if let Some(code) = s.chars().next().and_then(control_code) {
                    return Some(alt_prefix(alt, vec![code]));
                }
            }
            alt_prefix(alt, typed.unwrap_or(s).as_bytes().to_vec())
        }
        _ => return None,
    };
    Some(seq)
}

fn control_code(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

/// What a keybinding does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    NextColorScheme,
    OpenSettings,
}

/// A parsed key combo such as `ctrl+shift+c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyCombo {
    ctrl: bool,
    shift: bool,
    alt: bool,
    /// Lowercase key name: a character (`c`, `=`) or a named key
    /// (`pageup`, `home`, `f5`, `insert`, `enter`…).
    key: String,
}

impl KeyCombo {
    pub fn parse(s: &str) -> Result<KeyCombo, String> {
        let mut combo = KeyCombo {
            ctrl: false,
            shift: false,
            alt: false,
            key: String::new(),
        };
        let parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).collect();
        let (key, modifiers) = parts.split_last().ok_or("empty key combo")?;
        for m in modifiers {
            match m.as_str() {
                "ctrl" | "control" => combo.ctrl = true,
                "shift" => combo.shift = true,
                "alt" => combo.alt = true,
                other => return Err(format!("unknown modifier `{other}` in `{s}`")),
            }
        }
        if key.is_empty() {
            return Err(format!("no key in `{s}`"));
        }
        combo.key = key.clone();
        Ok(combo)
    }

    /// `key` should be the key *without* modifiers applied (so Ctrl+Shift+C
    /// arrives as `c`, not `C`).
    pub fn matches(&self, key: &Key, mods: ModifiersState) -> bool {
        if self.ctrl != mods.control_key()
            || self.shift != mods.shift_key()
            || self.alt != mods.alt_key()
        {
            return false;
        }
        key_name(key).is_some_and(|name| name == self.key)
    }
}

/// Lowercase name of a key, as used in config combos.
fn key_name(key: &Key) -> Option<String> {
    match key {
        Key::Character(s) => Some(s.to_lowercase()),
        Key::Named(NamedKey::Space) => Some("space".to_string()),
        Key::Named(NamedKey::ArrowUp) => Some("up".to_string()),
        Key::Named(NamedKey::ArrowDown) => Some("down".to_string()),
        Key::Named(NamedKey::ArrowLeft) => Some("left".to_string()),
        Key::Named(NamedKey::ArrowRight) => Some("right".to_string()),
        // PageUp -> "pageup", F5 -> "f5", Insert -> "insert"…
        Key::Named(named) => Some(format!("{named:?}").to_lowercase()),
        _ => None,
    }
}

/// The keybinding table built from config.
pub struct Bindings(Vec<(KeyCombo, Action)>);

impl Bindings {
    /// Build from config. Bad combos are skipped and reported.
    pub fn from_config(kb: &crate::config::Keybindings) -> (Bindings, Vec<String>) {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for (combo, action) in [
            (&kb.copy, Action::Copy),
            (&kb.paste, Action::Paste),
            (&kb.scroll_page_up, Action::ScrollPageUp),
            (&kb.scroll_page_down, Action::ScrollPageDown),
            (&kb.scroll_to_top, Action::ScrollToTop),
            (&kb.scroll_to_bottom, Action::ScrollToBottom),
            (&kb.next_color_scheme, Action::NextColorScheme),
            (&kb.open_settings, Action::OpenSettings),
        ] {
            if combo.trim().is_empty() {
                continue;
            }
            match KeyCombo::parse(combo) {
                Ok(c) => out.push((c, action)),
                Err(e) => errors.push(format!("keybinding: {e}")),
            }
        }
        (Bindings(out), errors)
    }

    pub fn lookup(&self, key: &Key, mods: ModifiersState) -> Option<Action> {
        self.0
            .iter()
            .find(|(combo, _)| combo.matches(key, mods))
            .map(|(_, a)| *a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    const NONE: ModifiersState = ModifiersState::empty();

    #[test]
    fn printable_and_control() {
        assert_eq!(
            encode_key(&ch("a"), Some("a"), NONE, false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            encode_key(&ch("c"), None, ModifiersState::CONTROL, false),
            Some(vec![3])
        );
        assert_eq!(
            encode_key(&ch("x"), Some("x"), ModifiersState::ALT, false),
            Some(b"\x1bx".to_vec())
        );
        // AltGr (Ctrl+Alt on Windows) producing '@' just types '@'.
        assert_eq!(
            encode_key(
                &ch("@"),
                Some("@"),
                ModifiersState::CONTROL | ModifiersState::ALT,
                false
            ),
            Some(b"@".to_vec())
        );
        assert_eq!(
            encode_key(&ch("2"), None, ModifiersState::CONTROL, false),
            Some(vec![0])
        );
        assert_eq!(
            encode_key(
                &ch("€"),
                Some("€"),
                ModifiersState::CONTROL | ModifiersState::ALT,
                false
            ),
            Some("€".as_bytes().to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Space), Some(" "), NONE, false),
            Some(b" ".to_vec())
        );
    }

    #[test]
    fn cursor_keys() {
        let up = Key::Named(NamedKey::ArrowUp);
        assert_eq!(encode_key(&up, None, NONE, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&up, None, NONE, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(
            encode_key(&up, None, ModifiersState::CONTROL, true),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::F5),
                None,
                ModifiersState::SHIFT,
                false
            ),
            Some(b"\x1b[15;2~".to_vec())
        );
    }

    #[test]
    fn bindings() {
        let (b, errs) = Bindings::from_config(&crate::config::Keybindings::default());
        assert!(errs.is_empty());
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(b.lookup(&ch("c"), cs), Some(Action::Copy));
        assert_eq!(b.lookup(&ch("c"), ModifiersState::CONTROL), None);
        assert_eq!(
            b.lookup(&Key::Named(NamedKey::PageUp), ModifiersState::SHIFT),
            Some(Action::ScrollPageUp)
        );
        assert_eq!(
            b.lookup(&ch(","), ModifiersState::CONTROL),
            Some(Action::OpenSettings)
        );
        assert!(KeyCombo::parse("hyper+x").is_err());
    }
}
