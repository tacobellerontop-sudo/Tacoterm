//! tacoshell: a native command shell for Windows, the companion to tacoterm.
//!
//! `tacoshell` starts an interactive shell, `tacoshell -c "line"` runs one
//! line, and `tacoshell file.tsh` runs a file of lines.

mod commands;
mod complete;
mod editor;
mod img;
mod parse;
mod resolve;
mod shell;
mod term;

use shell::Shell;
use std::io::{BufRead, IsTerminal};

const USAGE: &str = "\
usage: tacoshell              start an interactive shell
       tacoshell -c LINE...   run a command line and exit with its code
       tacoshell FILE         run each line of FILE
       tacoshell --version";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut shell = Shell::new();
    term::ignore_ctrl_c();
    let code = match args.first().map(String::as_str) {
        Some("-c") | Some("/c") => shell.run_line(&args[1..].join(" ")),
        Some("--version") | Some("-v") => {
            println!("tacoshell {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help") | Some("-h") | Some("/?") => {
            println!("{USAGE}");
            0
        }
        Some(file) => {
            let mut err = Vec::new();
            let code = shell.source(std::path::Path::new(file), &mut err);
            eprint!("{}", String::from_utf8_lossy(&err));
            code
        }
        None if std::io::stdin().is_terminal() => interactive(&mut shell),
        None => {
            // Lines piped in: run them in order.
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                shell.run_line(&line);
                if shell.exit.is_some() {
                    break;
                }
            }
            shell.status
        }
    };
    std::process::exit(shell.exit.unwrap_or(code));
}

fn interactive(shell: &mut Shell) -> i32 {
    term::enable_vt_output();
    if let Some(dir) = shell::config_dir() {
        shell.use_history_file(dir.join("history"));
        let init = dir.join("init.tsh");
        if init.is_file() {
            let mut err = Vec::new();
            shell.source(&init, &mut err);
            eprint!("{}", String::from_utf8_lossy(&err));
            shell.status = 0;
        }
    }
    println!(
        "\x1b[2mtacoshell {} \u{2014} type help for help\x1b[0m",
        env!("CARGO_PKG_VERSION")
    );
    while shell.exit.is_none() {
        print!("{}", shell.announce());
        let prompt = shell.prompt();
        let read = {
            let raw = match term::RawMode::enter() {
                Ok(raw) => raw,
                Err(e) => {
                    eprintln!("tacoshell: can't read keys from the console: {e}");
                    return 1;
                }
            };
            let history = shell.history.clone();
            let complete = |chars: &[char]| {
                let cwd = std::env::current_dir().unwrap_or_default();
                complete::complete(chars, &cwd, shell::home_dir().as_deref(), &|| {
                    shell.command_names()
                })
            };
            let read = editor::read_line(&mut term::Console, &prompt, &history, &complete);
            drop(raw);
            read
        };
        match read {
            Ok(editor::ReadLine::Line(line)) => {
                shell.remember(&line);
                if !line.trim().is_empty() {
                    shell.run_line(&line);
                    term::enable_vt_output();
                }
            }
            Ok(editor::ReadLine::Interrupted) => shell.status = 130,
            Ok(editor::ReadLine::Eof) => break,
            Err(e) => {
                eprintln!("tacoshell: {e}");
                return 1;
            }
        }
    }
    shell.status
}
