//! Running command lines: expansion, pipelines, redirections, programs and
//! the built-in commands.

use crate::parse::{self, Connector, Part, Pipeline, Redirect, Script, Word};
use crate::resolve;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{PipeReader, PipeWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

pub const BUILTINS: &[&str] = &[
    "alias", "cd", "clear", "cls", "echo", "env", "exit", "export", "false", "help", "history",
    "ls", "pwd", "set", "source", "true", "unalias", "unset", "which",
];

const HISTORY_MAX: usize = 1000;

pub struct Shell {
    /// The last exit code (`$?`).
    pub status: i32,
    pub aliases: HashMap<String, String>,
    pub history: Vec<String>,
    /// Set by `exit`.
    pub exit: Option<i32>,
    history_file: Option<PathBuf>,
    prev_dir: Option<PathBuf>,
}

/// A command with its words expanded.
struct Expanded {
    args: Vec<String>,
    redirects: Vec<Redir>,
}

enum Redir {
    Out { fd: u8, append: bool, path: String },
    In(String),
    ErrToOut,
}

/// Where a command's output goes.
enum Sink {
    Inherit,
    File(File),
    Pipe(PipeWriter),
}

impl Sink {
    fn try_clone(&self) -> std::io::Result<Sink> {
        Ok(match self {
            Sink::Inherit => Sink::Inherit,
            Sink::File(f) => Sink::File(f.try_clone()?),
            Sink::Pipe(p) => Sink::Pipe(p.try_clone()?),
        })
    }

    fn stdio(self) -> Stdio {
        match self {
            Sink::Inherit => Stdio::inherit(),
            Sink::File(f) => Stdio::from(f),
            Sink::Pipe(p) => Stdio::from(p),
        }
    }
}

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `%APPDATA%\tacoshell` on Windows, `~/.config/tacoshell` elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("tacoshell"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".config")))
            .map(|d| d.join("tacoshell"))
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

fn is_assignment(arg: &str) -> Option<(&str, &str)> {
    let (name, value) = arg.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some((name, value))
}

/// Paths the user sees: the home folder shortened to `~`.
pub fn tilde(path: &Path) -> String {
    let shown = path.display().to_string();
    if let Some(home) = home_dir() {
        let home = home.display().to_string();
        let same = if cfg!(windows) {
            shown.to_lowercase().starts_with(&home.to_lowercase())
        } else {
            shown.starts_with(&home)
        };
        let rest = shown.get(home.len()..).unwrap_or("");
        if same && (rest.is_empty() || rest.starts_with(['/', '\\'])) {
            return format!("~{rest}");
        }
    }
    shown
}

impl Shell {
    pub fn new() -> Shell {
        Shell {
            status: 0,
            aliases: HashMap::new(),
            history: Vec::new(),
            exit: None,
            history_file: None,
            prev_dir: None,
        }
    }

    /// Load history from `file` and append new lines to it from now on.
    pub fn use_history_file(&mut self, file: PathBuf) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            let lines: Vec<String> = text
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
            let keep = lines.len().saturating_sub(HISTORY_MAX);
            self.history = lines[keep..].to_vec();
            if lines.len() > HISTORY_MAX * 2 {
                let _ = std::fs::write(&file, self.history.join("\n") + "\n");
            }
        }
        self.history_file = Some(file);
    }

    pub fn remember(&mut self, line: &str) {
        let line = line.trim_end();
        // A leading space keeps a line out of history, as in bash.
        if line.trim().is_empty()
            || line.starts_with(' ')
            || self.history.last().is_some_and(|l| l == line)
        {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
        if let Some(file) = &self.history_file {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(file) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Tells the terminal the folder (OSC 9;9, which tacoterm and Windows
    /// Terminal use to open new tabs there) and the tab title. Sent once
    /// per prompt, not on every redraw.
    pub fn announce(&self) -> String {
        let cwd = std::env::current_dir().unwrap_or_default();
        let title = cwd
            .file_name()
            .map_or_else(|| tilde(&cwd), |n| n.to_string_lossy().into_owned());
        format!("\x1b]9;9;{}\x1b\\\x1b]0;{title}\x07", cwd.display())
    }

    /// The prompt: the folder in blue and a ❯ that turns red after a
    /// failure.
    pub fn prompt(&self) -> String {
        let shown = tilde(&std::env::current_dir().unwrap_or_default());
        let arrow = if self.status == 0 { 32 } else { 31 };
        format!("\x1b[1;34m{shown}\x1b[0m \x1b[{arrow}m\u{276F}\x1b[0m ")
    }

    /// Names that complete in command position: built-ins, aliases and
    /// programs on PATH.
    pub fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
        names.extend(self.aliases.keys().cloned());
        if cfg!(windows) {
            names.extend(resolve::CMD_BUILTINS.iter().map(|s| s.to_string()));
        }
        let exts = resolve::path_exts();
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if exts.is_empty() {
                    if resolve::find_in(&name, &dir, dir.as_os_str(), &exts).is_some() {
                        names.push(name);
                    }
                } else if let Some((stem, ext)) = name.rsplit_once('.') {
                    if exts.iter().any(|e| e[1..].eq_ignore_ascii_case(ext)) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names
    }

    /// Run one line. Returns the exit code, also kept in `status`.
    pub fn run_line(&mut self, line: &str) -> i32 {
        match parse::parse(line, &self.aliases) {
            Ok(script) => self.run_script(&script),
            Err(e) => {
                eprintln!("tacoshell: {e}");
                self.status = 2;
                2
            }
        }
    }

    pub fn run_script(&mut self, script: &Script) -> i32 {
        let mut run_next = true;
        for (pipeline, connector) in &script.items {
            if run_next {
                self.status = self.run_pipeline(pipeline);
            }
            if self.exit.is_some() {
                break;
            }
            run_next = match connector {
                Connector::Then => true,
                Connector::And => self.status == 0,
                Connector::Or => self.status != 0,
            };
        }
        self.status
    }

    fn expand(&self, word: &Word) -> String {
        let mut out = String::new();
        for part in &word.0 {
            match part {
                Part::Lit(s) => out.push_str(s),
                Part::Var(name) => out.push_str(&std::env::var(name).unwrap_or_default()),
                Part::Status => out.push_str(&self.status.to_string()),
                Part::Home => {
                    if let Some(home) = home_dir() {
                        out.push_str(&home.display().to_string());
                    } else {
                        out.push('~');
                    }
                }
            }
        }
        out
    }

    fn expand_command(&self, command: &parse::Command) -> Expanded {
        Expanded {
            args: command.words.iter().map(|w| self.expand(w)).collect(),
            redirects: command
                .redirects
                .iter()
                .map(|r| match r {
                    Redirect::Out { fd, append, target } => Redir::Out {
                        fd: *fd,
                        append: *append,
                        path: self.expand(target),
                    },
                    Redirect::In(target) => Redir::In(self.expand(target)),
                    Redirect::ErrToOut => Redir::ErrToOut,
                })
                .collect(),
        }
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline) -> i32 {
        let commands: Vec<Expanded> = pipeline
            .commands
            .iter()
            .map(|c| self.expand_command(c))
            .collect();
        let n = commands.len();
        let mut children: Vec<Option<Child>> = Vec::new();
        let mut writers = Vec::new();
        let mut statuses = Vec::new();
        let mut prev: Option<PipeReader> = None;

        for (i, cmd) in commands.into_iter().enumerate() {
            let last = i + 1 == n;
            let mut next: Option<PipeReader> = None;
            let stage = (|| -> Result<Option<i32>, (String, i32)> {
                let fail = |what: &str, e: std::io::Error| (format!("{what}: {e}"), 1);
                // Where input comes from and output goes.
                let mut stdin = prev.take().map(Stdio::from);
                let mut stdout = Sink::Inherit;
                let mut stderr = Sink::Inherit;
                let mut err_to_out = false;
                for r in &cmd.redirects {
                    match r {
                        Redir::In(path) => {
                            stdin = Some(Stdio::from(File::open(path).map_err(|e| fail(path, e))?))
                        }
                        Redir::Out { fd, append, path } => {
                            let file = OpenOptions::new()
                                .write(true)
                                .create(true)
                                .append(*append)
                                .truncate(!append)
                                .open(path)
                                .map_err(|e| fail(path, e))?;
                            if *fd == 1 {
                                stdout = Sink::File(file);
                            } else {
                                stderr = Sink::File(file);
                            }
                        }
                        Redir::ErrToOut => err_to_out = true,
                    }
                }
                if !last && matches!(stdout, Sink::Inherit) {
                    let (reader, writer) = std::io::pipe().map_err(|e| fail("pipe", e))?;
                    next = Some(reader);
                    stdout = Sink::Pipe(writer);
                }
                if err_to_out {
                    stderr = stdout.try_clone().map_err(|e| fail("2>&1", e))?;
                }

                let Some(name) = cmd.args.first() else {
                    return Ok(Some(0));
                };
                if let Some(status) =
                    self.try_builtin(&cmd.args, &mut stdout, &mut stderr, &mut writers)
                {
                    return Ok(Some(status));
                }
                let mut command = self.program(name, &cmd.args[1..]).map_err(|e| (e, 127))?;
                command.stdout(stdout.stdio()).stderr(stderr.stdio());
                if let Some(stdin) = stdin {
                    command.stdin(stdin);
                }
                let child = command.spawn().map_err(|e| (format!("{name}: {e}"), 126))?;
                children.push(Some(child));
                Ok(None)
            })();
            match stage {
                Ok(None) => statuses.push(None),
                Ok(Some(status)) => {
                    children.push(None);
                    statuses.push(Some(status));
                }
                Err((message, status)) => {
                    eprintln!("tacoshell: {message}");
                    children.push(None);
                    statuses.push(Some(status));
                }
            }
            prev = next;
        }
        drop(prev);

        let mut status = 0;
        for (child, known) in children.into_iter().zip(statuses) {
            status = match (child, known) {
                (Some(mut child), _) => child.wait().map(exit_code).unwrap_or(1),
                (None, Some(s)) => s,
                (None, None) => 1,
            };
        }
        for writer in writers {
            let _ = writer.join();
        }
        status
    }

    /// How to start a program: found on PATH (PowerShell scripts through
    /// PowerShell), or one of cmd's own commands through `cmd /c`.
    fn program(&self, name: &str, args: &[String]) -> Result<std::process::Command, String> {
        if let Some(path) = resolve::find(name) {
            let is_ps1 = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
            if is_ps1 {
                let ps = resolve::find("pwsh").unwrap_or_else(|| PathBuf::from("powershell.exe"));
                let mut c = std::process::Command::new(ps);
                c.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&path)
                .args(args);
                return Ok(c);
            }
            let mut c = std::process::Command::new(path);
            c.args(args);
            return Ok(c);
        }
        #[cfg(windows)]
        if resolve::is_cmd_builtin(name) {
            use std::os::windows::process::CommandExt;
            let quoted: Vec<String> = std::iter::once(name.to_string())
                .chain(args.iter().map(|a| {
                    if a.is_empty() || a.contains([' ', '&', '|', '<', '>', '^', '(', ')']) {
                        format!("\"{a}\"")
                    } else {
                        a.clone()
                    }
                }))
                .collect();
            let cmd = resolve::find("cmd").unwrap_or_else(|| PathBuf::from("cmd.exe"));
            let mut c = std::process::Command::new(cmd);
            c.raw_arg(format!("/d /c {}", quoted.join(" ")));
            return Ok(c);
        }
        Err(format!("command not found: {name}"))
    }

    /// Run `args` if it's a built-in (or an assignment, or a drive change);
    /// `None` if it's a program.
    fn try_builtin(
        &mut self,
        args: &[String],
        stdout: &mut Sink,
        stderr: &mut Sink,
        writers: &mut Vec<std::thread::JoinHandle<()>>,
    ) -> Option<i32> {
        let name = args[0].as_str();
        let lower = name.to_lowercase();
        let is_drive = cfg!(windows)
            && name.len() == 2
            && name.ends_with(':')
            && name.as_bytes()[0].is_ascii_alphabetic();
        let assignment = args.len() == 1 && is_assignment(name).is_some();
        let builtin = if cfg!(windows) { lower.as_str() } else { name };
        if !(BUILTINS.contains(&builtin) || builtin == "cd.." || is_drive || assignment) {
            return None;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let status = if assignment {
            let (k, v) = is_assignment(name).unwrap();
            set_var(k, v);
            0
        } else if is_drive {
            self.cd(&[name.to_string()], &mut out, &mut err)
        } else if builtin == "cd.." {
            self.cd(&["..".to_string()], &mut out, &mut err)
        } else {
            self.builtin(builtin, &args[1..], &mut out, &mut err)
        };
        // Output is gathered first and then handed over, so a builtin
        // feeding a pipe can't block on a reader that hasn't started yet.
        for (bytes, sink, is_err) in [(out, stdout, false), (err, stderr, true)] {
            if bytes.is_empty() {
                continue;
            }
            match std::mem::replace(sink, Sink::Inherit) {
                Sink::Inherit if is_err => {
                    let _ = std::io::stderr().write_all(&bytes);
                }
                Sink::Inherit => {
                    let mut o = std::io::stdout().lock();
                    let _ = o.write_all(&bytes).and_then(|_| o.flush());
                }
                Sink::File(mut f) => {
                    let _ = f.write_all(&bytes);
                }
                Sink::Pipe(mut p) => writers.push(std::thread::spawn(move || {
                    let _ = p.write_all(&bytes);
                })),
            }
        }
        Some(status)
    }

    fn builtin(
        &mut self,
        name: &str,
        args: &[String],
        out: &mut Vec<u8>,
        err: &mut Vec<u8>,
    ) -> i32 {
        match name {
            "cd" => self.cd(args, out, err),
            "pwd" => {
                let _ = writeln!(
                    out,
                    "{}",
                    std::env::current_dir().unwrap_or_default().display()
                );
                0
            }
            "echo" => {
                let (newline, words) = match args.first().map(String::as_str) {
                    Some("-n") => (false, &args[1..]),
                    _ => (true, args),
                };
                let _ = write!(
                    out,
                    "{}{}",
                    words.join(" "),
                    if newline { "\n" } else { "" }
                );
                0
            }
            "exit" => match args.first().map(|a| a.parse::<i32>()) {
                None => {
                    self.exit = Some(self.status);
                    self.status
                }
                Some(Ok(code)) => {
                    self.exit = Some(code);
                    code
                }
                Some(Err(_)) => {
                    let _ = writeln!(err, "exit: not a number: {}", args[0]);
                    2
                }
            },
            "set" | "export" if args.is_empty() => self.builtin("env", args, out, err),
            "set" | "export" => {
                let mut status = 0;
                for arg in args {
                    match is_assignment(arg) {
                        Some((k, v)) => set_var(k, v),
                        None => match std::env::var(arg) {
                            Ok(v) => {
                                let _ = writeln!(out, "{arg}={v}");
                            }
                            Err(_) => {
                                let _ = writeln!(err, "{name}: use NAME=value (no {arg} set)");
                                status = 1;
                            }
                        },
                    }
                }
                status
            }
            "unset" => {
                for arg in args {
                    unsafe { std::env::remove_var(arg) };
                }
                0
            }
            "env" => {
                let mut vars: Vec<(String, String)> = std::env::vars().collect();
                vars.sort_by_key(|(k, _)| k.to_lowercase());
                for (k, v) in vars {
                    let _ = writeln!(out, "{k}={v}");
                }
                0
            }
            "which" => {
                let mut status = 0;
                for arg in args {
                    let key = parse::alias_key(arg);
                    let lower = if cfg!(windows) {
                        arg.to_lowercase()
                    } else {
                        arg.clone()
                    };
                    if let Some(value) = self.aliases.get(&key) {
                        let _ = writeln!(out, "{arg}: alias for {value}");
                    } else if BUILTINS.contains(&lower.as_str()) {
                        let _ = writeln!(out, "{arg}: tacoshell built-in");
                    } else if let Some(path) = resolve::find(arg) {
                        let _ = writeln!(out, "{}", path.display());
                    } else if resolve::is_cmd_builtin(arg) {
                        let _ = writeln!(out, "{arg}: cmd.exe built-in");
                    } else {
                        let _ = writeln!(err, "which: {arg} not found");
                        status = 1;
                    }
                }
                status
            }
            "alias" if args.is_empty() => {
                let mut names: Vec<_> = self.aliases.iter().collect();
                names.sort();
                for (k, v) in names {
                    let _ = writeln!(out, "alias {k}='{v}'");
                }
                0
            }
            "alias" => {
                let mut status = 0;
                for arg in args {
                    match arg.split_once('=') {
                        Some((k, v)) if !k.is_empty() => {
                            self.aliases.insert(parse::alias_key(k), v.to_string());
                        }
                        _ => match self.aliases.get(&parse::alias_key(arg)) {
                            Some(v) => {
                                let _ = writeln!(out, "alias {arg}='{v}'");
                            }
                            None => {
                                let _ = writeln!(err, "alias: {arg} not found");
                                status = 1;
                            }
                        },
                    }
                }
                status
            }
            "unalias" => {
                for arg in args {
                    self.aliases.remove(&parse::alias_key(arg));
                }
                0
            }
            "history" if args.first().is_some_and(|a| a == "-c") => {
                self.history.clear();
                if let Some(file) = &self.history_file {
                    let _ = std::fs::write(file, "");
                }
                0
            }
            "history" => {
                for (i, line) in self.history.iter().enumerate() {
                    let _ = writeln!(out, "{:5}  {line}", i + 1);
                }
                0
            }
            "clear" | "cls" => {
                let _ = write!(out, "\x1b[H\x1b[2J\x1b[3J");
                0
            }
            "true" => 0,
            "false" => 1,
            "help" => {
                let _ = out.write_all(HELP.as_bytes());
                0
            }
            "source" => match args.first() {
                Some(file) => self.source(Path::new(file), err),
                None => {
                    let _ = writeln!(err, "source: which file?");
                    2
                }
            },
            "ls" => ls(args, out, err),
            _ => unreachable!("{name} is in BUILTINS"),
        }
    }

    fn cd(&mut self, args: &[String], out: &mut Vec<u8>, err: &mut Vec<u8>) -> i32 {
        // cmd's `cd /d D:\x` switches drive too; that's what cd does anyway.
        let args: Vec<&String> = args
            .iter()
            .filter(|a| !a.eq_ignore_ascii_case("/d"))
            .collect();
        let target = match args.first().map(|a| a.as_str()) {
            None => match home_dir() {
                Some(h) => h,
                None => return 0,
            },
            Some("-") => match &self.prev_dir {
                Some(p) => {
                    let _ = writeln!(out, "{}", tilde(p));
                    p.clone()
                }
                None => {
                    let _ = writeln!(err, "cd: no previous folder");
                    return 1;
                }
            },
            Some(dir) => PathBuf::from(dir),
        };
        let here = std::env::current_dir().ok();
        match std::env::set_current_dir(&target) {
            Ok(()) => {
                self.prev_dir = here;
                0
            }
            Err(e) => {
                let _ = writeln!(err, "cd: {}: {e}", target.display());
                1
            }
        }
    }

    /// Run each line of a file; stops at `exit`.
    pub fn source(&mut self, file: &Path, err: &mut Vec<u8>) -> i32 {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                let _ = writeln!(err, "tacoshell: {}: {e}", file.display());
                return 1;
            }
        };
        for line in text.lines() {
            self.run_line(line);
            if self.exit.is_some() {
                break;
            }
        }
        self.status
    }
}

impl Default for Shell {
    fn default() -> Self {
        Shell::new()
    }
}

fn set_var(name: &str, value: &str) {
    // The shell is single-threaded when it runs builtins (the only other
    // threads just copy bytes into pipes).
    unsafe {
        if value.is_empty() {
            std::env::remove_var(name);
        } else {
            std::env::set_var(name, value);
        }
    }
}

fn ls(args: &[String], out: &mut Vec<u8>, err: &mut Vec<u8>) -> i32 {
    let all = args.iter().any(|a| a == "-a");
    let mut paths: Vec<&str> = args
        .iter()
        .filter(|a| *a != "-a")
        .map(String::as_str)
        .collect();
    if paths.is_empty() {
        paths.push(".");
    }
    let mut status = 0;
    for (n, path) in paths.iter().enumerate() {
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(e)
                if Path::new(path).is_file()
                    && e.kind() != std::io::ErrorKind::PermissionDenied =>
            {
                let _ = writeln!(out, "{path}");
                continue;
            }
            Err(e) => {
                let _ = writeln!(err, "ls: {path}: {e}");
                status = 1;
                continue;
            }
        };
        if paths.len() > 1 {
            let _ = writeln!(out, "{}{path}:", if n > 0 { "\n" } else { "" });
        }
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| all || !name.starts_with('.'))
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        for name in names {
            let _ = writeln!(out, "{name}");
        }
    }
    status
}

const HELP: &str = "\
tacoshell: a command shell for Windows

  a | b        pipe a's output into b
  a && b       run b if a worked;  a || b  run b if a failed
  a ; b        run both (a & b works too, as in cmd)
  > f  >> f    send output to file f (>> appends); 2> f for errors; 2>&1
  < f          read input from file f
  'text'       taken as is;  \"text\"  expands $VAR and %VAR%
  `x           a literal x (backslash is just a path separator)
  $VAR %VAR%   environment variables;  $?  the last exit code;  ~  home

Built-ins:
  cd [dir|-]   change folder (no dir: home; -: the previous one; D: works too)
  ls [-a]      list a folder        pwd     show the current folder
  echo [-n]    print words          clear   clear the screen (or cls)
  set NAME=value  (or NAME=value)   unset NAME    env    list variables
  alias name='cmd args'   unalias name   which name   history [-c]
  source file  run a file of commands       exit [code]

cmd.exe's own commands (dir, copy, del, type, mklink, start, ...) run through
cmd. %APPDATA%\\tacoshell\\init.tsh runs at startup, for aliases and such.

Keys: Tab completes, Up/Down history, Ctrl+Left/Right by word, Ctrl+W/U/K
delete a word / to start / to end, Esc clears the line, Ctrl+L clears the
screen, Ctrl+C cancels, Ctrl+D on an empty line exits.
";
