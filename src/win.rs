//! Small Win32 helpers: the window handle, see-through frames, the bell,
//! opening links and the global summon hotkey.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MB_OK, MSG, SW_SHOWNORMAL, WM_HOTKEY};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

pub fn hwnd(window: &Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

/// Let the system backdrop (acrylic, mica) fill the whole client area
/// instead of just the title bar.
pub fn extend_frame(window: &Window, on: bool) {
    let Some(hwnd) = hwnd(window) else { return };
    let m = if on { -1 } else { 0 };
    let margins = MARGINS {
        cxLeftWidth: m,
        cxRightWidth: m,
        cyTopHeight: m,
        cyBottomHeight: m,
    };
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

pub fn beep() {
    unsafe {
        let _ = MessageBeep(MB_OK);
    }
}

/// Open a link with its default handler. Only web, mail and file links
/// get here (see `Grid::url_at`), never a bare command.
pub fn open_url(url: &str) {
    let lower = url.to_ascii_lowercase();
    let allowed = ["http://", "https://", "ftp://", "file://", "mailto:"];
    if !allowed.iter().any(|p| lower.starts_with(p)) {
        return;
    }
    unsafe {
        ShellExecuteW(
            HWND::default(),
            &HSTRING::from("open"),
            &HSTRING::from(url),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Parse `win+\``, `ctrl+alt+t`, `ctrl+shift+f12`… into RegisterHotKey's
/// modifiers and virtual-key code.
fn parse_hotkey(spec: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let parts: Vec<String> = spec.split('+').map(|p| p.trim().to_lowercase()).collect();
    // "ctrl++" splits into a trailing empty part: the key is "+".
    let (key, mods) = match parts.as_slice() {
        [mods @ .., a, b] if a.is_empty() && b.is_empty() => ("+".to_string(), mods.to_vec()),
        [mods @ .., key] => (key.clone(), mods.to_vec()),
        [] => return Err("empty hotkey".to_string()),
    };
    let mut m = MOD_NOREPEAT;
    for part in &mods {
        m |= match part.as_str() {
            "win" | "super" | "windows" => MOD_WIN,
            "ctrl" | "control" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            other => return Err(format!("unknown modifier `{other}` in `{spec}`")),
        };
    }
    let vk = match key.as_str() {
        "`" | "backtick" | "grave" | "~" => 0xC0, // VK_OEM_3
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "escape" | "esc" => 0x1B,
        "-" | "minus" => 0xBD,
        "=" | "+" | "plus" => 0xBB,
        "," | "comma" => 0xBC,
        "." | "period" => 0xBE,
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
            k.to_ascii_uppercase().as_bytes()[0] as u32
        }
        k if k.starts_with('f') && k[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
            0x70 + k[1..].parse::<u32>().unwrap() - 1
        }
        other => return Err(format!("unknown key `{other}` in `{spec}`")),
    };
    Ok((m, vk))
}

/// Register a system-wide hotkey on a thread of its own and call
/// `pressed` each time it fires. Fails when the combo is malformed or
/// another program already owns it.
pub fn register_hotkey(spec: &str, pressed: impl Fn() + Send + 'static) -> Result<(), String> {
    let (mods, vk) = parse_hotkey(spec)?;
    let spec = spec.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        // A hotkey registered without a window posts to this thread's queue.
        if RegisterHotKey(HWND::default(), 1, mods, vk).is_err() {
            let _ = tx.send(Err(format!(
                "summon_hotkey \"{spec}\" is already used by another program"
            )));
            return;
        }
        let _ = tx.send(Ok(()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                pressed();
            }
        }
    });
    rx.recv().unwrap_or(Err("hotkey thread died".to_string()))
}
