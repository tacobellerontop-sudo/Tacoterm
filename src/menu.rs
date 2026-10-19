//! The right-click menu: a native Win32 popup, so it looks and behaves like
//! every other Windows context menu.

use windows::core::HSTRING;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow, TrackPopupMenu,
    HMENU, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    TPM_RETURNCMD, TPM_RIGHTBUTTON,
};
use winit::window::Window;

use tacoterm::config::{Backdrop, ShellChoice};
use tacoterm::schemes::SCHEMES;

pub enum Choice {
    NewTab,
    SplitRight,
    SplitDown,
    ClosePane,
    Find,
    Copy,
    Paste,
    SelectAll,
    Scheme(&'static str),
    Opacity(f32),
    Backdrop(Backdrop),
    /// A `shell =` setting.
    Shell(&'static str),
    Fullscreen,
    Settings,
}

/// What the menu shows checked or grayed.
pub struct State<'a> {
    pub has_selection: bool,
    pub scheme: &'a str,
    pub shells: &'a [ShellChoice],
    pub shell: &'a str,
    pub opacity: f32,
    pub backdrop: Backdrop,
    pub fullscreen: bool,
}

const NEW_TAB: usize = 1;
const SPLIT_RIGHT: usize = 2;
const SPLIT_DOWN: usize = 3;
const CLOSE_PANE: usize = 4;
const FIND: usize = 5;
const COPY: usize = 6;
const PASTE: usize = 7;
const SELECT_ALL: usize = 8;
const FULLSCREEN: usize = 9;
const SETTINGS: usize = 10;
const SCHEME_BASE: usize = 100;
const SHELL_BASE: usize = 200;
const OPACITY_BASE: usize = 300;
const BACKDROP_BASE: usize = 400;

const OPACITIES: [u32; 6] = [100, 90, 80, 70, 60, 50];
const BACKDROPS: [(Backdrop, &str); 4] = [
    (Backdrop::None, "None"),
    (Backdrop::Acrylic, "Acrylic (frosted glass)"),
    (Backdrop::Mica, "Mica"),
    (Backdrop::Tabbed, "Mica Alt"),
];

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

unsafe fn submenu(menu: HMENU, sub: HMENU, text: &str) {
    let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &HSTRING::from(text));
}

unsafe fn separator(menu: HMENU) {
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
}

/// Show the menu at the mouse and wait for a pick.
pub fn show(window: &Window, state: &State) -> Option<Choice> {
    let hwnd = crate::win::hwnd(window)?;
    let none = MENU_ITEM_FLAGS(0);
    let checked = |on: bool| if on { MF_CHECKED } else { none };
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let schemes = CreatePopupMenu().ok()?;
        let shell_menu = CreatePopupMenu().ok()?;
        let opacity = CreatePopupMenu().ok()?;
        let backdrop = CreatePopupMenu().ok()?;

        item(menu, none, NEW_TAB, "New tab\tCtrl+Shift+T");
        item(menu, none, SPLIT_RIGHT, "Split right\tAlt+Shift+=");
        item(menu, none, SPLIT_DOWN, "Split down\tAlt+Shift+-");
        item(menu, none, CLOSE_PANE, "Close\tCtrl+Shift+W");
        separator(menu);
        let copy_flags = if state.has_selection { none } else { MF_GRAYED };
        item(menu, copy_flags, COPY, "Copy\tCtrl+Shift+C");
        item(menu, none, PASTE, "Paste\tCtrl+Shift+V");
        item(menu, none, SELECT_ALL, "Select all\tCtrl+Shift+A");
        item(menu, none, FIND, "Find…\tCtrl+Shift+F");
        separator(menu);

        for (i, s) in SCHEMES.iter().enumerate() {
            item(
                schemes,
                checked(s.name == state.scheme),
                SCHEME_BASE + i,
                &pretty(s.name),
            );
        }
        submenu(menu, schemes, "Color scheme");

        let current = (state.opacity * 100.0).round() as u32;
        for (i, pct) in OPACITIES.iter().enumerate() {
            let label = if *pct == 100 {
                "Solid".to_string()
            } else {
                format!("{pct}%")
            };
            item(opacity, checked(*pct == current), OPACITY_BASE + i, &label);
        }
        submenu(menu, opacity, "Opacity");

        for (i, (b, label)) in BACKDROPS.iter().enumerate() {
            item(
                backdrop,
                checked(*b == state.backdrop),
                BACKDROP_BASE + i,
                label,
            );
        }
        submenu(menu, backdrop, "Background effect");

        for (i, s) in state.shells.iter().enumerate() {
            item(
                shell_menu,
                checked(s.setting == state.shell),
                SHELL_BASE + i,
                s.label,
            );
        }
        submenu(menu, shell_menu, "Shell");
        item(
            menu,
            checked(state.fullscreen),
            FULLSCREEN,
            "Full screen\tF11",
        );

        separator(menu);
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

        let in_range = |base: usize, len: usize| (base..base + len).contains(&picked);
        match picked {
            NEW_TAB => Some(Choice::NewTab),
            SPLIT_RIGHT => Some(Choice::SplitRight),
            SPLIT_DOWN => Some(Choice::SplitDown),
            CLOSE_PANE => Some(Choice::ClosePane),
            FIND => Some(Choice::Find),
            COPY => Some(Choice::Copy),
            PASTE => Some(Choice::Paste),
            SELECT_ALL => Some(Choice::SelectAll),
            FULLSCREEN => Some(Choice::Fullscreen),
            SETTINGS => Some(Choice::Settings),
            n if in_range(SCHEME_BASE, SCHEMES.len()) => {
                Some(Choice::Scheme(SCHEMES[n - SCHEME_BASE].name))
            }
            n if in_range(SHELL_BASE, state.shells.len()) => {
                Some(Choice::Shell(state.shells[n - SHELL_BASE].setting))
            }
            n if in_range(OPACITY_BASE, OPACITIES.len()) => {
                Some(Choice::Opacity(OPACITIES[n - OPACITY_BASE] as f32 / 100.0))
            }
            n if in_range(BACKDROP_BASE, BACKDROPS.len()) => {
                Some(Choice::Backdrop(BACKDROPS[n - BACKDROP_BASE].0))
            }
            _ => None,
        }
    }
}
