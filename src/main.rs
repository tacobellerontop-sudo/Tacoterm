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
mod menu;
#[cfg(windows)]
mod pty;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    // `tacoterm --cwd DIR` (or just `tacoterm DIR`, as Explorer's "Open in
    // tacoterm" passes it) starts the shell there.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // `tacoterm --use-shell PATH` (the installer's "use tacoshell") saves
    // PATH as the shell in config.toml and exits.
    if args.first().is_some_and(|a| a == "--use-shell") {
        let Some(path) = args.get(1).and_then(|p| p.to_str()) else {
            std::process::exit(2);
        };
        let value = format!("'\"{}\"'", path.trim_matches('"'));
        if let Err(e) = tacoterm::config::set_top_level("shell", &value) {
            app::show_error(&format!("cannot save the shell: {e}"));
            std::process::exit(1);
        }
        return;
    }
    let dir = match args.first().and_then(|a| a.to_str()) {
        Some("--cwd") => args.get(1),
        Some(_) => args.first(),
        None => None,
    };
    if let Some(dir) = dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            app::show_error(&format!("cannot open {}: {e}", dir.to_string_lossy()));
        }
    }
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
