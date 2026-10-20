//! tacoshell: a native command shell for Windows, the companion to tacoterm.
//!
//! `tacoshell` starts an interactive shell, `tacoshell -c "line"` runs one
//! line, and `tacoshell file.tsh` runs a file of lines.

mod arith;
mod commands;
mod complete;
mod editor;
mod glob;
mod highlight;
mod img;
mod jump;
mod parse;
mod prompt;
mod resolve;
mod shell;
mod term;

use shell::Shell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Instant;

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
        // Lines piped in: run them in order.
        None => shell.run_lines(std::io::stdin().lock().lines().map_while(Result::ok)),
    };
    std::process::exit(shell.exit.unwrap_or(code));
}

fn interactive(shell: &mut Shell) -> i32 {
    term::enable_vt_output();
    if let Some(dir) = shell::config_dir() {
        shell.use_history_file(dir.join("history"));
        shell.use_jump_file(dir.join("jumps"));
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
        // A prompt over several lines: the editor redraws only the last.
        let full = shell.prompt().replace("\r\n", "\n");
        let prompt = match full.rsplit_once('\n') {
            Some((above, last)) => {
                print!("{}\r\n", above.replace('\n', "\r\n"));
                last.to_string()
            }
            None => full,
        };
        let _ = std::io::stdout().flush();
        let mut text = String::new();
        let mut flat = String::new();
        let mut this_prompt = prompt;
        let line = loop {
            match read(shell, &this_prompt) {
                Ok(editor::ReadLine::Line(line)) => {
                    if text.is_empty() {
                        text = line.clone();
                        flat = line;
                    } else {
                        // Remember it as one line: `a |` + `b` is `a | b`,
                        // `if x` + `y` is `if x; y`.
                        let joiner = if ["|", "&&", "||"]
                            .iter()
                            .any(|op| flat.trim_end().ends_with(op))
                        {
                            " "
                        } else {
                            "; "
                        };
                        flat = format!("{}{joiner}{}", flat.trim_end(), line.trim_start());
                        text = format!("{text}\n{line}");
                    }
                    if parse::is_incomplete(&text, &shell.aliases) {
                        this_prompt = "\x1b[90m\u{2026}\x1b[0m ".to_string();
                        continue;
                    }
                    break Some(text);
                }
                Ok(editor::ReadLine::Interrupted) => {
                    shell.status = 130;
                    break None;
                }
                Ok(editor::ReadLine::Eof) => return shell.status,
                Err(e) => {
                    eprintln!("tacoshell: {e}");
                    return 1;
                }
            }
        };
        let Some(mut line) = line else { continue };
        if let Some(expanded) = shell::expand_history(&line, &shell.history) {
            println!("\x1b[2m{expanded}\x1b[0m");
            line = expanded.clone();
            flat = expanded;
        }
        // A quote left open over lines keeps its newlines.
        let remembered = if parse::parse(&flat, &shell.aliases).is_ok() {
            &flat
        } else {
            &line
        };
        shell.remember(&remembered.clone());
        if !line.trim().is_empty() {
            shell::INTERRUPTED.store(false, std::sync::atomic::Ordering::Relaxed);
            let start = Instant::now();
            shell.run_line(&line);
            shell.took = Some(start.elapsed());
            term::enable_vt_output();
        }
    }
    shell.status
}

/// Read a line from the console with editing, completion and colors.
fn read(shell: &Shell, prompt: &str) -> std::io::Result<editor::ReadLine> {
    let _raw = term::RawMode::enter().map_err(|e| {
        std::io::Error::new(e.kind(), format!("can't read keys from the console: {e}"))
    })?;
    let complete = |chars: &[char]| {
        let cwd = std::env::current_dir().unwrap_or_default();
        complete::complete(chars, &cwd, shell::home_dir().as_deref(), &|| {
            shell.command_names()
        })
    };
    // Looking a program up on PATH takes a moment, so each name is looked up
    // once per line.
    let known: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    let is_command = |name: &str| {
        if let Some(&k) = known.borrow().get(name) {
            return k;
        }
        let k = shell.knows(name) || resolve::find(name).is_some();
        known.borrow_mut().insert(name.to_string(), k);
        k
    };
    let highlight = |chars: &[char]| highlight::highlight(chars, &is_command);
    editor::read_line(
        &mut term::Console,
        prompt,
        &shell.history,
        &complete,
        &highlight,
    )
}
