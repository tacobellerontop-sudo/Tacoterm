//! The right-click menu: a native Win32 popup, so it looks and behaves like
//! every other Windows context menu.

use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow, TrackPopupMenu,
    HMENU, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    TPM_RETURNCMD, TPM_RIGHTBUTTON,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use tacoterm::config::ShellChoice;
use tacoterm::schemes::SCHEMES;

pub enum Choice {
    Copy,
    Paste,
    Scheme(&'static str),
    /// A `shell =` setting.
    Shell(&'static str),
    Settings,
}

const COPY: usize = 1;
const PASTE: usize = 2;
const SETTINGS: usize = 3;
const SCHEME_BASE: usize = 100;
const SHELL_BASE: usize = 200;

/// "tokyo-night" -> "Tokyo Night".
fn pretty(name: &str) -> String {
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

unsafe fn item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let _ = AppendMenuW(menu, MF_STRING | flags, id, &HSTRING::from(text));
}

/// Show the menu at the mouse and wait for a pick.
pub fn show(
    window: &Window,
    has_selection: bool,
    scheme: &str,
    shells: &[ShellChoice],
    shell: &str,
) -> Option<Choice> {
    let hwnd = match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut core::ffi::c_void),
        _ => return None,
    };
    let none = MENU_ITEM_FLAGS(0);
    let checked = |on: bool| if on { MF_CHECKED } else { none };
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let schemes = CreatePopupMenu().ok()?;
        let shell_menu = CreatePopupMenu().ok()?;

        let copy_flags = if has_selection { none } else { MF_GRAYED };
        item(menu, copy_flags, COPY, "Copy\tCtrl+Shift+C");
        item(menu, none, PASTE, "Paste\tCtrl+Shift+V");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);

        for (i, s) in SCHEMES.iter().enumerate() {
            item(
                schemes,
                checked(s.name == scheme),
                SCHEME_BASE + i,
                &pretty(s.name),
            );
        }
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            schemes.0 as usize,
            &HSTRING::from("Color scheme"),
        );

        for (i, s) in shells.iter().enumerate() {
            item(
                shell_menu,
                checked(s.setting == shell),
                SHELL_BASE + i,
                s.label,
            );
        }
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            shell_menu.0 as usize,
            &HSTRING::from("Shell"),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        item(menu, none, SETTINGS, "All settings…\tCtrl+,");

        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Without this the menu doesn't close when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            at.x,
            at.y,
            0,
            hwnd,
            None,
        )
        .0 as usize;
        // Destroying the parent destroys the submenus.
        let _ = DestroyMenu(menu);

        match picked {
            COPY => Some(Choice::Copy),
            PASTE => Some(Choice::Paste),
            SETTINGS => Some(Choice::Settings),
            n if (SCHEME_BASE..SCHEME_BASE + SCHEMES.len()).contains(&n) => {
                Some(Choice::Scheme(SCHEMES[n - SCHEME_BASE].name))
            }
            n if (SHELL_BASE..SHELL_BASE + shells.len()).contains(&n) => {
                Some(Choice::Shell(shells[n - SHELL_BASE].setting))
            }
            _ => None,
        }
    }
}
