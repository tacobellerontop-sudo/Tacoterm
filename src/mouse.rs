//! Mouse and focus reporting: what to send the program when it has asked
//! for mouse events (`CSI ? 1000 h` and friends) or focus events (?1004).
//!
//! Pure encoding — the app decides which window events become which
//! [`MouseButton`] / [`MouseEventKind`], and writes the bytes to the PTY.

use crate::grid::MouseMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion with no button held.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Highest 1-based coordinate the legacy encoding can carry in one byte.
const LEGACY_MAX: usize = 255 - 32;

/// Encode one mouse event as the program asked for it. `col`/`row` are
/// 0-based viewport cells. Returns `None` when the mode says this event
/// isn't reported (tracking off, motion in Click mode, button-less motion
/// in Drag mode, a wheel "release"), or when the legacy encoding can't
/// represent the position.
pub fn encode(
    mode: MouseMode,
    sgr: bool,
    button: MouseButton,
    kind: MouseEventKind,
    col: usize,
    row: usize,
    mods: MouseMods,
) -> Option<Vec<u8>> {
    let wheel = matches!(button, MouseButton::WheelUp | MouseButton::WheelDown);
    let reported = match (mode, kind) {
        (MouseMode::Off, _) => false,
        (_, MouseEventKind::Release) => !wheel && button != MouseButton::None,
        (_, MouseEventKind::Press) => button != MouseButton::None,
        (MouseMode::Click, MouseEventKind::Motion) => false,
        (MouseMode::Drag, MouseEventKind::Motion) => button != MouseButton::None && !wheel,
        (MouseMode::Motion, MouseEventKind::Motion) => !wheel,
    };
    if !reported {
        return None;
    }

    let mut code: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if kind == MouseEventKind::Motion {
        code += 32;
    }
    code += 4 * mods.shift as u32 + 8 * mods.alt as u32 + 16 * mods.ctrl as u32;

    let (x, y) = (col + 1, row + 1);
    if sgr {
        // SGR keeps the button on release; the final byte says which it is.
        let fin = if kind == MouseEventKind::Release {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes());
    }
    if kind == MouseEventKind::Release {
        // Legacy releases don't say which button: code 3, modifiers kept.
        code = (code & !0b11) | 3;
    }
    if x > LEGACY_MAX || y > LEGACY_MAX {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        (32 + code) as u8,
        (32 + x) as u8,
        (32 + y) as u8,
    ])
}

/// The ?1004 report for the window gaining or losing focus.
pub fn focus_report(focused: bool) -> &'static [u8] {
    if focused {
        b"\x1b[I"
    } else {
        b"\x1b[O"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MouseButton as B;
    use MouseEventKind as K;

    const NO_MODS: MouseMods = MouseMods {
        shift: false,
        alt: false,
        ctrl: false,
    };

    fn enc(mode: MouseMode, sgr: bool, b: MouseButton, k: MouseEventKind) -> Option<Vec<u8>> {
        encode(mode, sgr, b, k, 4, 9, NO_MODS)
    }

    #[test]
    fn sgr_press_release_and_wheel() {
        let m = MouseMode::Click;
        assert_eq!(enc(m, true, B::Left, K::Press).unwrap(), b"\x1b[<0;5;10M");
        assert_eq!(
            enc(m, true, B::Right, K::Release).unwrap(),
            b"\x1b[<2;5;10m"
        );
        assert_eq!(enc(m, true, B::Middle, K::Press).unwrap(), b"\x1b[<1;5;10M");
        assert_eq!(
            enc(m, true, B::WheelUp, K::Press).unwrap(),
            b"\x1b[<64;5;10M"
        );
        assert_eq!(
            enc(m, true, B::WheelDown, K::Press).unwrap(),
            b"\x1b[<65;5;10M"
        );
        assert_eq!(enc(m, true, B::WheelUp, K::Release), None);
        // Coordinates beyond the legacy limit are fine in SGR.
        let far = encode(m, true, B::Left, K::Press, 300, 999, NO_MODS).unwrap();
        assert_eq!(far, b"\x1b[<0;301;1000M");
    }

    #[test]
    fn modifiers_add_to_the_code() {
        let mods = MouseMods {
            shift: true,
            alt: true,
            ctrl: true,
        };
        let e = encode(MouseMode::Click, true, B::Left, K::Press, 0, 0, mods).unwrap();
        assert_eq!(e, b"\x1b[<28;1;1M");
        let ctrl = MouseMods {
            ctrl: true,
            ..NO_MODS
        };
        let e = encode(MouseMode::Click, true, B::WheelDown, K::Press, 0, 0, ctrl).unwrap();
        assert_eq!(e, b"\x1b[<81;1;1M");
    }

    #[test]
    fn legacy_encoding() {
        let m = MouseMode::Click;
        assert_eq!(
            enc(m, false, B::Left, K::Press).unwrap(),
            b"\x1b[M\x20\x25\x2a"
        );
        // Release is always button 3.
        assert_eq!(
            enc(m, false, B::Right, K::Release).unwrap(),
            b"\x1b[M\x23\x25\x2a"
        );
        assert_eq!(
            enc(m, false, B::WheelUp, K::Press).unwrap(),
            b"\x1b[M\x60\x25\x2a"
        );
        // 1-based 223 is the last representable coordinate.
        let edge = encode(m, false, B::Left, K::Press, 222, 0, NO_MODS).unwrap();
        assert_eq!(edge, b"\x1b[M\x20\xff\x21");
        assert_eq!(encode(m, false, B::Left, K::Press, 223, 0, NO_MODS), None);
        assert_eq!(encode(m, false, B::Left, K::Press, 0, 500, NO_MODS), None);
    }

    #[test]
    fn modes_filter_events() {
        for b in [B::Left, B::WheelUp, B::None] {
            for k in [K::Press, K::Release, K::Motion] {
                assert_eq!(enc(MouseMode::Off, true, b, k), None);
            }
        }
        // Click: no motion at all.
        assert_eq!(enc(MouseMode::Click, true, B::Left, K::Motion), None);
        assert_eq!(enc(MouseMode::Click, true, B::None, K::Motion), None);
        // Drag: motion only with a button held.
        assert_eq!(
            enc(MouseMode::Drag, true, B::Left, K::Motion).unwrap(),
            b"\x1b[<32;5;10M"
        );
        assert_eq!(enc(MouseMode::Drag, true, B::None, K::Motion), None);
        // Motion: everything, button-less motion as 35.
        assert_eq!(
            enc(MouseMode::Motion, true, B::None, K::Motion).unwrap(),
            b"\x1b[<35;5;10M"
        );
        assert_eq!(
            enc(MouseMode::Motion, false, B::Right, K::Motion).unwrap(),
            b"\x1b[M\x42\x25\x2a"
        );
        // A "press" or "release" of no button means nothing.
        assert_eq!(enc(MouseMode::Motion, true, B::None, K::Press), None);
        assert_eq!(enc(MouseMode::Motion, true, B::None, K::Release), None);
    }

    #[test]
    fn focus_reports() {
        assert_eq!(focus_report(true), b"\x1b[I");
        assert_eq!(focus_report(false), b"\x1b[O");
    }
}
