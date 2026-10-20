//! The console: raw input for the line editor, its width, and Ctrl+C.

use std::io::{Read, Write};

/// Raw, VT input while it's alive; the console's own modes come back when
/// it's dropped, so programs the shell runs get the console as usual.
pub struct RawMode {
    #[cfg(windows)]
    input_mode: windows::Win32::System::Console::CONSOLE_MODE,
    #[cfg(unix)]
    termios: libc::termios,
}

#[cfg(windows)]
mod sys {
    use windows::Win32::Foundation::{BOOL, HANDLE, TRUE};
    use windows::Win32::System::Console::*;

    fn handle(which: STD_HANDLE) -> std::io::Result<HANDLE> {
        Ok(unsafe { GetStdHandle(which) }?)
    }

    pub fn enter_raw() -> std::io::Result<super::RawMode> {
        let input = handle(STD_INPUT_HANDLE)?;
        let mut mode = CONSOLE_MODE::default();
        unsafe { GetConsoleMode(input, &mut mode) }?;
        let raw = (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        unsafe { SetConsoleMode(input, raw) }?;
        enable_vt_output();
        Ok(super::RawMode { input_mode: mode })
    }

    pub fn leave_raw(raw: &super::RawMode) {
        if let Ok(input) = handle(STD_INPUT_HANDLE) {
            let _ = unsafe { SetConsoleMode(input, raw.input_mode) };
        }
    }

    /// Let escape sequences through to the console (programs may have
    /// turned it off).
    pub fn enable_vt_output() {
        if let Ok(output) = handle(STD_OUTPUT_HANDLE) {
            let mut mode = CONSOLE_MODE::default();
            if unsafe { GetConsoleMode(output, &mut mode) }.is_ok() {
                let _ = unsafe {
                    SetConsoleMode(
                        output,
                        mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT,
                    )
                };
            }
        }
    }

    pub fn columns() -> Option<usize> {
        let output = handle(STD_OUTPUT_HANDLE).ok()?;
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        unsafe { GetConsoleScreenBufferInfo(output, &mut info) }.ok()?;
        let cols = info.srWindow.Right - info.srWindow.Left + 1;
        (cols > 0).then_some(cols as usize)
    }

    unsafe extern "system" fn on_ctrl(_kind: u32) -> BOOL {
        // Handled: the shell stays up, and the program in front (which got
        // the same Ctrl+C) stops. A running loop stops too.
        crate::shell::INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
        TRUE
    }

    pub fn ignore_ctrl_c() {
        let _ = unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true) };
    }
}

#[cfg(unix)]
mod sys {
    pub fn enter_raw() -> std::io::Result<super::RawMode> {
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut termios) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut raw = termios;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSADRAIN, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(super::RawMode { termios })
        }
    }

    pub fn leave_raw(raw: &super::RawMode) {
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &raw.termios) };
    }

    pub fn enable_vt_output() {}

    pub fn columns() -> Option<usize> {
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            (libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0)
                .then_some(size.ws_col as usize)
        }
    }

    extern "C" fn on_sigint(_: libc::c_int) {
        crate::shell::INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn ignore_ctrl_c() {
        // A handler, not SIG_IGN, so programs we start get the default back.
        unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
    }
}

pub use sys::{enable_vt_output, ignore_ctrl_c};

impl RawMode {
    pub fn enter() -> std::io::Result<RawMode> {
        sys::enter_raw()
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        sys::leave_raw(self);
    }
}

pub fn columns() -> usize {
    sys::columns()
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(80)
}

/// The real console, for `editor::read_line`.
pub struct Console;

impl crate::editor::Io for Console {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match std::io::stdin().lock().read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    fn write(&mut self, s: &str) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(s.as_bytes())?;
        out.flush()
    }

    fn cols(&self) -> usize {
        columns()
    }
}
