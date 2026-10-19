//! tacoterm — a GPU-accelerated terminal emulator for Windows.
//!
//! The real console lives in a Windows pseudo-console (ConPTY); this process
//! is a fast window onto it: bytes flow PTY -> VT parser -> grid -> renderer,
//! and keystrokes flow back the other way.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("tacoterm drives the Windows pseudo-console (ConPTY) and only builds on Windows.");

mod app;
mod config;
mod grid;
mod input;
mod pty;
mod renderer;
mod vt;

fn main() {
    if let Err(e) = app::run() {
        eprintln!("tacoterm: {e}");
        std::process::exit(1);
    }
}
