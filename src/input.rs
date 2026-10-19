//! Keyboard → VT byte sequences.
//!
//! Special keys become CSI/SS3 sequences (with modifier parameters);
//! printable keys become their text, with Ctrl/Alt applied.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Modifier parameter for CSI sequences: 1 + shift(1) + alt(2) + ctrl(4).
fn mod_param(mods: &ModifiersState) -> u8 {
    1 + (mods.shift_key() as u8) + ((mods.alt_key() as u8) << 1) + ((mods.control_key() as u8) << 2)
}

/// `\x1b[X` style, or `\x1b[1;{m}X` with modifiers (arrows, Home, End).
fn csi(code: u8, mods: &ModifiersState) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        vec![0x1b, b'[', code]
    } else {
        vec![0x1b, b'[', b'1', b';', b'0' + m, code]
    }
}

/// `\x1b[{n}~` style, or `\x1b[{n};{m}~` (Insert, Delete, PgUp, PgDn, F5+).
fn tilde(code: &str, mods: &ModifiersState) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{m}~").into_bytes()
    }
}

/// SS3 (`\x1bO`) for F1-F4, with modifier support.
fn ss3(code: u8, mods: &ModifiersState) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        vec![0x1b, b'O', code]
    } else {
        vec![0x1b, b'[', b'1', b';', b'0' + m, code]
    }
}

/// Translate a key press into the bytes the PTY should receive.
/// Returns None for keys the terminal ignores (pure modifiers, etc.).
/// Key repeat is honored: holding a key repeats it, like a real terminal.
pub fn key_to_bytes(event: &KeyEvent, mods: &ModifiersState) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }
    let ctrl = mods.control_key();
    let alt = mods.alt_key();

    let seq = match &event.logical_key {
        Key::Named(NamedKey::Enter) => b"\r".to_vec(),
        Key::Named(NamedKey::Tab) => {
            if mods.shift_key() {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Key::Named(NamedKey::Backspace) => vec![0x7f],
        Key::Named(NamedKey::Escape) => vec![0x1b],
        Key::Named(NamedKey::ArrowUp) => csi(b'A', mods),
        Key::Named(NamedKey::ArrowDown) => csi(b'B', mods),
        Key::Named(NamedKey::ArrowRight) => csi(b'C', mods),
        Key::Named(NamedKey::ArrowLeft) => csi(b'D', mods),
        Key::Named(NamedKey::Home) => csi(b'H', mods),
        Key::Named(NamedKey::End) => csi(b'F', mods),
        Key::Named(NamedKey::Insert) => tilde("2", mods),
        Key::Named(NamedKey::Delete) => tilde("3", mods),
        Key::Named(NamedKey::PageUp) => tilde("5", mods),
        Key::Named(NamedKey::PageDown) => tilde("6", mods),
        Key::Named(NamedKey::F1) => ss3(b'P', mods),
        Key::Named(NamedKey::F2) => ss3(b'Q', mods),
        Key::Named(NamedKey::F3) => ss3(b'R', mods),
        Key::Named(NamedKey::F4) => ss3(b'S', mods),
        Key::Named(NamedKey::F5) => tilde("15", mods),
        Key::Named(NamedKey::F6) => tilde("17", mods),
        Key::Named(NamedKey::F7) => tilde("18", mods),
        Key::Named(NamedKey::F8) => tilde("19", mods),
        Key::Named(NamedKey::F9) => tilde("20", mods),
        Key::Named(NamedKey::F10) => tilde("21", mods),
        Key::Named(NamedKey::F11) => tilde("23", mods),
        Key::Named(NamedKey::F12) => tilde("24", mods),
        Key::Character(s) => {
            // Ctrl+letter -> C0 control code (Ctrl+C = 0x03, etc.)
            if ctrl {
                if let Some(c) = s.chars().next() {
                    let lower = c.to_ascii_lowercase();
                    if ('a'..='z').contains(&lower) {
                        let mut v = vec![(lower as u8) - b'a' + 1];
                        if alt {
                            v.insert(0, 0x1b);
                        }
                        return Some(v);
                    }
                    let code = match lower {
                        '[' | '{' => 0x1b,
                        '\\' | '|' => 0x1c,
                        ']' | '}' => 0x1d,
                        '^' => 0x1e,
                        '_' | '-' => 0x1f,
                        _ => return Some(s.as_bytes().to_vec()),
                    };
                    let mut v = vec![code];
                    if alt {
                        v.insert(0, 0x1b);
                    }
                    return Some(v);
                }
            }
            let mut v = Vec::new();
            if alt {
                v.push(0x1b); // Alt+key = ESC prefix
            }
            v.extend_from_slice(s.as_bytes());
            v
        }
        _ => return None,
    };
    Some(seq)
}
