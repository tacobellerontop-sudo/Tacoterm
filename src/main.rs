//! tacoterm — a GPU-accelerated terminal emulator for Windows.
//!
//! The real console lives in a Windows pseudo-console (ConPTY); this process
//! is a fast window onto it: bytes flow PTY -> VT parser -> grid -> renderer,
//! and keystrokes flow back the other way. The terminal core lives in the
//! library (`src/lib.rs`); this binary adds the window and ConPTY.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod pty;

#[cfg(windows)]
fn main() {
    if let Err(e) = app::run() {
        app::show_error(&e);
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("tacoterm drives the Windows pseudo-console (ConPTY) and only runs on Windows.");
    std::process::exit(1);
}
