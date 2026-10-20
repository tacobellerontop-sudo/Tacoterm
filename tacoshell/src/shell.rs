//! Running command lines: expansion, pipelines, redirections, blocks,
//! functions, programs and the built-in commands.

use crate::commands::{self, Input, Source};
use crate::glob::{self, Tok};
use crate::jump::Jumps;
use crate::parse::{self, Connector, Part, Pipeline, Redirect, Script, Stmt, Word};
use crate::{arith, prompt, resolve};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const BUILTINS: &[&str] = &[
    "[",
    "alias",
    "break",
    "cd",
    "clear",
    "cls",
    "continue",
    "dirs",
    "echo",
    "env",
    "exit",
    "export",
    "false",
    "functions",
    "help",
    "history",
    "ls",
    "math",
    "popd",
    "pushd",
    "pwd",
    "read",
    "return",
    "set",
    "source",
    "test",
    "time",
    "true",
    "type",
    "unalias",
    "unset",
    "which",
    "z",
];

/// The shell's own commands and the everyday ones in `commands`.
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name) || commands::NAMES.contains(&name)
}

/// Set by Ctrl+C, so a loop stops instead of starting its next round.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

const HISTORY_MAX: usize = 1000;

/// Functions calling functions this deep are surely stuck.
const MAX_DEPTH: usize = 200;

/// Newlines inside a history entry, as stored in the history file (which
/// has one entry per line).
const NEWLINE_MARK: char = '\u{1f}';

#[derive(Debug, Clone, Copy, PartialEq)]
enum Flow {
    Break,
    Continue,
    Return,
}

pub struct Shell {
    /// The last exit code (`$?`).
    pub status: i32,
    pub aliases: HashMap<String, String>,
    pub functions: HashMap<String, Script>,
    pub history: Vec<String>,
    /// Set by `exit`.
    pub exit: Option<i32>,
    /// How long the last command line took, for the prompt.
    pub took: Option<Duration>,
    history_file: Option<PathBuf>,
    prev_dir: Option<PathBuf>,
    /// `pushd` / `popd`
    dir_stack: Vec<PathBuf>,
    jumps: Jumps,
    /// Whether the running built-in's output goes straight to the terminal
    /// (then text gets a box around it).
    to_terminal: bool,
    /// Each running function's arguments (`$1`, `$argv`).
    args: Vec<Vec<String>>,
    /// How many loops are running, for `break` and `continue`.
    loops: usize,
    flow: Option<Flow>,
    /// Where output goes instead of the console: the pipe a `$(…)` reads,
    /// or the next stage after a function in a pipeline.
    capture: Option<Sink>,
    /// A function in a pipeline gets the stage's input.
    fn_stdin: Option<Source>,
    /// The capture pipe's reader hasn't started yet, so built-ins writing
    /// into it mustn't be waited for.
    unread_pipe: bool,
    /// Threads handing built-in output to pipes.
    writers: Vec<std::thread::JoinHandle<()>>,
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

/// One word being expanded: its text, and the same as a file name pattern
/// (wildcards that came from quotes or variables stay plain characters).
#[derive(Clone, Default)]
struct Field {
    text: String,
    pat: Vec<Tok>,
    wild: bool,
}

impl Field {
    fn lit(s: &str) -> Field {
        Field {
            text: s.to_string(),
            pat: s.chars().map(Tok::Char).collect(),
            wild: false,
        }
    }

    fn join(&self, other: &Field) -> Field {
        let mut f = self.clone();
        f.text.push_str(&other.text);
        f.pat.extend_from_slice(&other.pat);
        f.wild |= other.wild;
        f
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

/// Whether a word is `NAME=value`.
pub fn is_assignment_word(word: &str) -> bool {
    is_assignment(word).is_some()
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

/// `!!` (the last line) and `!$` (its last word), outside single quotes.
/// `None` when there's nothing to expand.
pub fn expand_history(line: &str, history: &[String]) -> Option<String> {
    if !line.contains("!!") && !line.contains("!$") {
        return None;
    }
    let last = history.last()?;
    let last_word = last.split_whitespace().last().unwrap_or("");
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    let mut changed = false;
    while let Some(c) = chars.next() {
        if c == '\'' {
            quoted = !quoted;
        }
        if c == '!' && !quoted {
            match chars.peek() {
                Some('!') => {
                    chars.next();
                    out.push_str(last);
                    changed = true;
                    continue;
                }
                Some('$') => {
                    chars.next();
                    out.push_str(last_word);
                    changed = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    changed.then_some(out)
}

/// Quote `arg` so the parser reads it back as one plain word.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && !arg.contains(|c: char| c.is_whitespace() || "'\"`|&;<>$%*?{}~#()".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'`''"))
    }
}

impl Shell {
    pub fn new() -> Shell {
        Shell {
            status: 0,
            aliases: HashMap::new(),
            functions: HashMap::new(),
            history: Vec::new(),
            exit: None,
            took: None,
            history_file: None,
            prev_dir: None,
            dir_stack: Vec::new(),
            jumps: Jumps::default(),
            to_terminal: false,
            args: Vec::new(),
            loops: 0,
            flow: None,
            capture: None,
            fn_stdin: None,
            unread_pipe: false,
            writers: Vec::new(),
        }
    }

    /// Load history from `file` and append new lines to it from now on.
    pub fn use_history_file(&mut self, file: PathBuf) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            let lines: Vec<String> = text
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.replace(NEWLINE_MARK, "\n"))
                .collect();
            let keep = lines.len().saturating_sub(HISTORY_MAX);
            self.history = lines[keep..].to_vec();
            if lines.len() > HISTORY_MAX * 2 {
                let kept: Vec<String> = self
                    .history
                    .iter()
                    .map(|l| l.replace('\n', &NEWLINE_MARK.to_string()))
                    .collect();
                let _ = std::fs::write(&file, kept.join("\n") + "\n");
            }
        }
        self.history_file = Some(file);
    }

    /// Remember the folders `cd` goes to, for `z`, in `file`.
    pub fn use_jump_file(&mut self, file: PathBuf) {
        self.jumps = Jumps::load(file);
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
                let _ = writeln!(f, "{}", line.replace('\n', &NEWLINE_MARK.to_string()));
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

    /// The prompt: a `prompt` function's output if there is one, else the
    /// `TACOSHELL_PROMPT` template, else folder, git branch, time taken and
    /// a ❯ that turns red after a failure.
    pub fn prompt(&mut self) -> String {
        let status = self.status;
        if self.functions.contains_key(&parse::alias_key("prompt")) {
            let out = self.capture_output("prompt");
            self.status = status;
            return out.trim_end_matches(['\r', '\n']).to_string();
        }
        let template = std::env::var("TACOSHELL_PROMPT")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| prompt::DEFAULT.to_string());
        let cwd = std::env::current_dir().unwrap_or_default();
        prompt::render(
            &template,
            &prompt::Info {
                cwd: &cwd,
                status,
                took: self.took,
            },
        )
    }

    /// Names that complete in command position: built-ins, functions,
    /// aliases and programs on PATH.
    pub fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTINS
            .iter()
            .chain(commands::NAMES)
            .map(|s| s.to_string())
            .collect();
        names.extend(self.aliases.keys().cloned());
        names.extend(self.functions.keys().cloned());
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

    /// Whether `name` would run something: for coloring the line as it's
    /// typed. Doesn't look at PATH (the caller does, and caches it).
    pub fn knows(&self, name: &str) -> bool {
        let key = parse::alias_key(name);
        let lower = if cfg!(windows) {
            name.to_lowercase()
        } else {
            name.to_string()
        };
        is_builtin(&lower)
            || self.aliases.contains_key(&key)
            || self.functions.contains_key(&key)
            || resolve::is_cmd_builtin(name)
            || is_assignment(name).is_some()
            || auto_cd(name)
    }

    /// Run one line (which may hold several, separated by newlines).
    /// Returns the exit code, also kept in `status`.
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

    /// Run lines from a file or a pipe; a command spread over several lines
    /// (an open block, quote or trailing `|`) runs once it's whole.
    pub fn run_lines(&mut self, lines: impl Iterator<Item = String>) -> i32 {
        let mut pending = String::new();
        for line in lines {
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(&line);
            if parse::is_incomplete(&pending, &self.aliases) {
                continue;
            }
            self.run_line(&std::mem::take(&mut pending));
            if self.exit.is_some() {
                return self.status;
            }
        }
        if !pending.is_empty() {
            self.run_line(&pending);
        }
        self.status
    }

    pub fn run_script(&mut self, script: &Script) -> i32 {
        let mut run_next = true;
        for (stmt, connector) in &script.items {
            if run_next {
                self.status = self.run_stmt(stmt);
            }
            if self.exit.is_some() || self.flow.is_some() {
                break;
            }
            if interrupted() {
                self.status = 130;
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

    /// Whether a loop should stop after its body: `break`, `return`,
    /// `exit` or Ctrl+C. `continue` just ends this round.
    fn end_round(&mut self) -> bool {
        match self.flow {
            Some(Flow::Break) => {
                self.flow = None;
                true
            }
            Some(Flow::Continue) => {
                self.flow = None;
                false
            }
            Some(Flow::Return) => true,
            None => self.exit.is_some() || interrupted(),
        }
    }

    fn run_stmt(&mut self, stmt: &Stmt) -> i32 {
        match stmt {
            Stmt::Pipeline(p) => {
                let status = self.run_pipeline(p);
                if p.negate {
                    (status == 0) as i32
                } else {
                    status
                }
            }
            Stmt::If {
                cond,
                then,
                otherwise,
            } => {
                let ok = self.run_script(cond) == 0;
                if self.exit.is_some() || self.flow.is_some() || interrupted() {
                    return self.status;
                }
                match (ok, otherwise) {
                    (true, _) => self.run_script(then),
                    (false, Some(other)) => self.run_script(other),
                    (false, None) => 0,
                }
            }
            Stmt::While { cond, body } => {
                self.loops += 1;
                let mut status = 0;
                loop {
                    let ok = self.run_script(cond) == 0;
                    if !ok || self.exit.is_some() || self.flow.is_some() || interrupted() {
                        break;
                    }
                    status = self.run_script(body);
                    if self.end_round() {
                        break;
                    }
                }
                self.loops -= 1;
                if interrupted() {
                    130
                } else {
                    status
                }
            }
            Stmt::For { var, items, body } => {
                let mut values = Vec::new();
                for item in items {
                    values.extend(self.expand_word(item));
                }
                self.loops += 1;
                let mut status = 0;
                for value in values {
                    set_var(var, &value);
                    status = self.run_script(body);
                    if self.end_round() {
                        break;
                    }
                }
                self.loops -= 1;
                if interrupted() {
                    130
                } else {
                    status
                }
            }
            Stmt::Function { name, body } => {
                self.functions.insert(parse::alias_key(name), body.clone());
                0
            }
        }
    }

    /// A variable: a function argument (`$1`, `$#`) or the environment.
    fn var(&self, name: &str) -> Option<String> {
        let args = self.args.last();
        if name == "#" {
            return Some(args.map_or(0, |a| a.len()).to_string());
        }
        if let Ok(n) = name.parse::<usize>() {
            return match n {
                0 => Some("tacoshell".into()),
                n => args.and_then(|a| a.get(n - 1)).cloned(),
            };
        }
        std::env::var(name).ok()
    }

    fn expand_parts(&mut self, parts: &[Part]) -> Vec<Field> {
        let mut fields = vec![Field::default()];
        for part in parts {
            let values: Vec<Field> = match part {
                Part::Lit(s) => vec![Field::lit(s)],
                Part::Wild(c) => vec![Field {
                    text: c.to_string(),
                    pat: vec![if *c == '*' { Tok::Star } else { Tok::One }],
                    wild: true,
                }],
                Part::Var(name) => vec![Field::lit(&self.var(name).unwrap_or_default())],
                Part::Status => vec![Field::lit(&self.status.to_string())],
                Part::Home => vec![Field::lit(
                    &home_dir().map_or("~".into(), |h| h.display().to_string()),
                )],
                Part::AllArgs { quoted } => {
                    let args = self.args.last().cloned().unwrap_or_default();
                    if *quoted {
                        vec![Field::lit(&args.join(" "))]
                    } else {
                        args.iter().map(|a| Field::lit(a)).collect()
                    }
                }
                Part::Brace(choices) => {
                    let mut all = Vec::new();
                    for choice in choices {
                        all.extend(self.expand_parts(&choice.0));
                    }
                    all
                }
                Part::Subst { script, quoted } => {
                    let out = self.capture_output(script);
                    if *quoted {
                        vec![Field::lit(out.trim_end_matches(['\r', '\n']))]
                    } else {
                        out.lines()
                            .map(|l| l.trim_end_matches('\r'))
                            .filter(|l| !l.is_empty())
                            .map(Field::lit)
                            .collect()
                    }
                }
                Part::Arith(expr) => {
                    match arith::eval(&self.expand_math(expr), &|name| self.var(name)) {
                        Ok(n) => vec![Field::lit(&n.to_string())],
                        Err(e) => {
                            eprintln!("tacoshell: $(({expr})): {e}");
                            vec![Field::default()]
                        }
                    }
                }
            };
            // Several values make several words (`{a,b}x` is `ax bx`); none
            // makes none.
            fields = fields
                .iter()
                .flat_map(|f| values.iter().map(move |v| f.join(v)))
                .collect();
        }
        fields
    }

    /// Arithmetic text with `$(…)` replaced by its output (nested `$((…))`
    /// is just parentheses).
    fn expand_math(&mut self, expr: &str) -> String {
        let mut out = String::new();
        let mut rest = expr;
        while let Some(at) = rest.find("$(") {
            out.push_str(&rest[..at]);
            let after = &rest[at + 2..];
            if after.starts_with('(') {
                out.push('(');
                rest = after;
                continue;
            }
            let mut depth = 1;
            let end = after.char_indices().find_map(|(i, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(i)
            });
            let Some(end) = end else {
                out.push_str(&rest[at..]);
                return out;
            };
            out.push_str(self.capture_output(&after[..end]).trim());
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// A word's expansion: usually one word, but braces, `$argv` and
    /// `$(…)` can make several, and wildcards become the matching files
    /// (or stay as typed when nothing matches).
    fn expand_word(&mut self, word: &Word) -> Vec<String> {
        let mut out = Vec::new();
        for field in self.expand_parts(&word.0) {
            if field.wild {
                let found = glob::glob(&field.pat);
                if !found.is_empty() {
                    out.extend(found);
                    continue;
                }
            }
            out.push(field.text);
        }
        out
    }

    /// A redirection's target: one file name.
    fn expand_one(&mut self, word: &Word) -> String {
        self.expand_word(word).join(" ")
    }

    /// Run `script` and return what it printed.
    fn capture_output(&mut self, script: &str) -> String {
        let Ok((mut reader, writer)) = std::io::pipe() else {
            return String::new();
        };
        let reading = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        });
        let saved_capture = self.capture.replace(Sink::Pipe(writer));
        let saved_unread = std::mem::replace(&mut self.unread_pipe, false);
        let saved_exit = self.exit.take();
        let saved_flow = self.flow.take();
        self.run_line(script);
        self.exit = saved_exit;
        self.flow = saved_flow;
        self.unread_pipe = saved_unread;
        // Dropping our end of the pipe lets the reader finish.
        self.capture = saved_capture;
        String::from_utf8_lossy(&reading.join().unwrap_or_default()).into_owned()
    }

    fn expand_command(&mut self, command: &parse::Command) -> Expanded {
        let mut args = Vec::new();
        for word in &command.words {
            args.extend(self.expand_word(word));
        }
        let mut redirects = Vec::new();
        for r in &command.redirects {
            redirects.push(match r {
                Redirect::Out { fd, append, target } => Redir::Out {
                    fd: *fd,
                    append: *append,
                    path: self.expand_one(target),
                },
                Redirect::In(target) => Redir::In(self.expand_one(target)),
                Redirect::ErrToOut => Redir::ErrToOut,
            });
        }
        Expanded { args, redirects }
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline) -> i32 {
        let mut commands = Vec::new();
        for c in &pipeline.commands {
            commands.push(self.expand_command(c));
        }
        let n = commands.len();
        let mut children: Vec<Option<Child>> = Vec::new();
        let first_writer = self.writers.len();
        let mut statuses = Vec::new();
        let mut prev: Option<PipeReader> = None;

        for (i, cmd) in commands.into_iter().enumerate() {
            let last = i + 1 == n;
            let mut next: Option<PipeReader> = None;
            // `NAME=value cmd`: the variables are set while cmd runs.
            let assigned = cmd
                .args
                .iter()
                .take_while(|a| is_assignment(a).is_some())
                .count();
            let only_assignments = assigned == cmd.args.len();
            let saved: Vec<(String, Option<String>)> = if only_assignments {
                Vec::new()
            } else {
                cmd.args[..assigned]
                    .iter()
                    .map(|a| {
                        let name = is_assignment(a).unwrap().0;
                        (name.to_string(), std::env::var(name).ok())
                    })
                    .collect()
            };
            for a in &cmd.args[..assigned] {
                let (k, v) = is_assignment(a).unwrap();
                set_var(k, v);
            }
            let args = if only_assignments {
                &[][..]
            } else {
                &cmd.args[assigned..]
            };

            let stage = (|| -> Result<Option<i32>, (String, i32)> {
                let fail = |what: &str, e: std::io::Error| (format!("{what}: {e}"), 1);
                // Where input comes from and output goes.
                let mut stdin = match prev.take() {
                    Some(p) => Some(Source::Pipe(p)),
                    None if i == 0 => self.fn_stdin.take(),
                    None => None,
                };
                let mut stdout = match (&self.capture, last) {
                    (Some(c), true) => c.try_clone().map_err(|e| fail("pipe", e))?,
                    _ => Sink::Inherit,
                };
                let mut stderr = Sink::Inherit;
                let mut err_to_out = false;
                for r in &cmd.redirects {
                    match r {
                        Redir::In(path) => {
                            stdin = Some(Source::File(File::open(path).map_err(|e| fail(path, e))?))
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

                let Some(name) = args.first() else {
                    return Ok(Some(0));
                };
                if let Some(body) = self.functions.get(&parse::alias_key(name)).cloned() {
                    return self.call(&body, args, stdin, stdout, !last).map(Some);
                }
                if let Some(status) = self.try_builtin(args, &mut stdin, &mut stdout, &mut stderr) {
                    return Ok(Some(status));
                }
                let mut command = self.program(name, &args[1..]).map_err(|e| (e, 127))?;
                command.stdout(stdout.stdio()).stderr(stderr.stdio());
                if let Some(stdin) = stdin {
                    command.stdin(stdin.stdio());
                }
                let child = command.spawn().map_err(|e| (format!("{name}: {e}"), 126))?;
                children.push(Some(child));
                Ok(None)
            })();
            for (k, v) in saved {
                match v {
                    Some(v) => set_var(&k, &v),
                    None => set_var(&k, ""),
                }
            }
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
        if !self.unread_pipe {
            for writer in self.writers.drain(first_writer..) {
                let _ = writer.join();
            }
        }
        status
    }

    /// Run a function with `args[1..]` as its arguments; its output goes to
    /// `stdout` (`piped`: the next stage of a pipeline, not reading yet).
    fn call(
        &mut self,
        body: &Script,
        args: &[String],
        stdin: Option<Source>,
        stdout: Sink,
        piped: bool,
    ) -> Result<i32, (String, i32)> {
        if self.args.len() >= MAX_DEPTH {
            return Err((format!("{}: functions nested too deep", args[0]), 1));
        }
        let capture = match stdout {
            Sink::Inherit => None,
            other => Some(other),
        };
        let saved_capture = std::mem::replace(&mut self.capture, capture);
        let saved_unread = self.unread_pipe;
        self.unread_pipe |= piped;
        let saved_stdin = std::mem::replace(&mut self.fn_stdin, stdin);
        let saved_loops = std::mem::take(&mut self.loops);
        self.args.push(args[1..].to_vec());
        let status = self.run_script(body);
        self.args.pop();
        if self.flow == Some(Flow::Return) {
            self.flow = None;
        }
        self.loops = saved_loops;
        self.fn_stdin = saved_stdin;
        self.unread_pipe = saved_unread;
        self.capture = saved_capture;
        Ok(status)
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

    /// Run `args` if it's a built-in (or a drive change, or a folder to go
    /// to); `None` if it's a program.
    fn try_builtin(
        &mut self,
        args: &[String],
        stdin: &mut Option<Source>,
        stdout: &mut Sink,
        stderr: &mut Sink,
    ) -> Option<i32> {
        let name = args[0].as_str();
        let lower = name.to_lowercase();
        let is_drive = cfg!(windows)
            && name.len() == 2
            && name.ends_with(':')
            && name.as_bytes()[0].is_ascii_alphabetic();
        let builtin = if cfg!(windows) { lower.as_str() } else { name };
        let go_to = args.len() == 1 && auto_cd(name) && resolve::find(name).is_none();
        if !(is_builtin(builtin) || builtin == "cd.." || is_drive || go_to) {
            return None;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let status = if is_drive || go_to {
            self.cd(&[name.to_string()], &mut out, &mut err)
        } else if builtin == "cd.." {
            self.cd(&["..".to_string()], &mut out, &mut err)
        } else {
            self.to_terminal = matches!(stdout, Sink::Inherit)
                && std::io::IsTerminal::is_terminal(&std::io::stdout());
            self.builtin(
                builtin,
                &args[1..],
                &mut Input(stdin.take()),
                &mut out,
                &mut err,
            )
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
                Sink::Pipe(mut p) => self.writers.push(std::thread::spawn(move || {
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
        input: &mut Input,
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
            "break" | "continue" => {
                if self.loops == 0 {
                    let _ = writeln!(err, "{name}: not inside a loop");
                    return 1;
                }
                self.flow = Some(if name == "break" {
                    Flow::Break
                } else {
                    Flow::Continue
                });
                0
            }
            "return" => {
                let code = match args.first().map(|a| a.parse::<i32>()) {
                    None => self.status,
                    Some(Ok(code)) => code,
                    Some(Err(_)) => {
                        let _ = writeln!(err, "return: not a number: {}", args[0]);
                        return 2;
                    }
                };
                if self.args.is_empty() {
                    let _ = writeln!(err, "return: not inside a function");
                    return 1;
                }
                self.flow = Some(Flow::Return);
                code
            }
            "test" | "[" => {
                let mut args = args;
                if name == "[" {
                    match args.split_last() {
                        Some((last, rest)) if last == "]" => args = rest,
                        _ => {
                            let _ = writeln!(err, "[: missing ]");
                            return 2;
                        }
                    }
                }
                match test(args) {
                    Ok(true) => 0,
                    Ok(false) => 1,
                    Err(e) => {
                        let _ = writeln!(err, "{name}: {e}");
                        2
                    }
                }
            }
            "math" => match arith::eval(&args.join(" "), &|n| self.var(n)) {
                Ok(n) => {
                    let _ = writeln!(out, "{n}");
                    0
                }
                Err(e) => {
                    let _ = writeln!(err, "math: {e}");
                    2
                }
            },
            "read" => self.read(args, input, err),
            "set" | "export" if args.is_empty() => self.builtin("env", args, input, out, err),
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
            "which" | "type" => {
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
                    } else if self.functions.contains_key(&key) {
                        let _ = writeln!(out, "{arg}: function (functions {arg} shows it)");
                    } else if is_builtin(&lower) {
                        let _ = writeln!(out, "{arg}: tacoshell built-in");
                    } else if let Some(path) = resolve::find(arg) {
                        let _ = writeln!(out, "{}", path.display());
                    } else if resolve::is_cmd_builtin(arg) {
                        let _ = writeln!(out, "{arg}: cmd.exe built-in");
                    } else {
                        let _ = writeln!(err, "{name}: {arg} not found");
                        status = 1;
                    }
                }
                status
            }
            "functions" if args.is_empty() => {
                let mut names: Vec<&String> = self.functions.keys().collect();
                names.sort();
                for n in names {
                    let _ = writeln!(out, "{n}");
                }
                0
            }
            "functions" => {
                let mut status = 0;
                for arg in args {
                    if arg == "-e" || arg == "--erase" {
                        continue;
                    }
                    let key = parse::alias_key(arg);
                    if args.iter().any(|a| a == "-e" || a == "--erase") {
                        self.functions.remove(&key);
                    } else if let Some(body) = self.functions.get(&key) {
                        let _ = writeln!(out, "function {arg}\n{}end", show_script(body, 1));
                    } else {
                        let _ = writeln!(err, "functions: no function {arg}");
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
                let filter = args.first().map(|a| a.to_lowercase());
                for (i, line) in self.history.iter().enumerate() {
                    if filter
                        .as_ref()
                        .is_some_and(|f| !line.to_lowercase().contains(f))
                    {
                        continue;
                    }
                    let _ = writeln!(out, "{:5}  {}", i + 1, line.replace('\n', "\n       "));
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
                Some(file) => {
                    let saved = std::mem::replace(&mut self.args, vec![args[1..].to_vec()]);
                    let status = self.source(Path::new(file), err);
                    self.args = saved;
                    status
                }
                None => {
                    let _ = writeln!(err, "source: which file?");
                    2
                }
            },
            "ls" => ls(args, out, err),
            "z" => self.z(args, out, err),
            "pushd" => {
                let here = std::env::current_dir().unwrap_or_default();
                let target = match args.first() {
                    Some(dir) => PathBuf::from(dir),
                    None => match self.dir_stack.pop() {
                        Some(top) => top,
                        None => {
                            let _ = writeln!(err, "pushd: no other folder");
                            return 1;
                        }
                    },
                };
                let status = self.cd(&[target.display().to_string()], out, err);
                if status == 0 {
                    self.dir_stack.push(here);
                    self.builtin("dirs", &[], input, out, err);
                }
                status
            }
            "popd" => match self.dir_stack.pop() {
                Some(dir) => {
                    let status = self.cd(&[dir.display().to_string()], out, err);
                    self.builtin("dirs", &[], input, out, err);
                    status
                }
                None => {
                    let _ = writeln!(err, "popd: the folder stack is empty");
                    1
                }
            },
            "dirs" => {
                let here = std::env::current_dir().unwrap_or_default();
                let all: Vec<String> = std::iter::once(&here)
                    .chain(self.dir_stack.iter().rev())
                    .map(|p| tilde(p))
                    .collect();
                let _ = writeln!(out, "{}", all.join(" "));
                0
            }
            "time" => {
                // Run the rest as a command line and say how long it took.
                let line: Vec<String> = args.iter().map(|a| quote(a)).collect();
                let start = std::time::Instant::now();
                let status = self.run_line(&line.join(" "));
                let _ = writeln!(err, "real {}", prompt::duration(start.elapsed()));
                status
            }
            _ => match commands::run(name, args, input, self.to_terminal, out, err) {
                Some(status) => status,
                None => unreachable!("{name} is a built-in"),
            },
        }
    }

    /// `read [-p PROMPT] NAME...`: one line of input into variables, a word
    /// each and the rest in the last.
    fn read(&mut self, args: &[String], input: &mut Input, err: &mut Vec<u8>) -> i32 {
        let mut names = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "-p" {
                if let Some(p) = it.next() {
                    eprint!("{p}");
                }
            } else {
                names.push(a.as_str());
            }
        }
        if names.is_empty() {
            let _ = writeln!(err, "read: which variable? read NAME");
            return 2;
        }
        let mut line = String::new();
        let got = match input.0.take() {
            Some(Source::Pipe(p)) => read_one_line(p, &mut line),
            Some(Source::File(f)) => read_one_line(f, &mut line),
            None => std::io::stdin().lock().read_line(&mut line).unwrap_or(0),
        };
        if got == 0 {
            return 1;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let mut rest = line.trim_start();
        for (i, name) in names.iter().enumerate() {
            let value = if i + 1 == names.len() {
                std::mem::take(&mut rest).trim_end()
            } else {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let (word, after) = rest.split_at(end);
                rest = after.trim_start();
                word
            };
            set_var(name, value);
        }
        0
    }

    fn z(&mut self, args: &[String], out: &mut Vec<u8>, err: &mut Vec<u8>) -> i32 {
        if args.is_empty() || args[0] == "-l" {
            for (score, path) in self.jumps.list().iter().take(20) {
                let _ = writeln!(out, "{score:8.1}  {}", tilde(path));
            }
            return 0;
        }
        match self.jumps.matches(args).first() {
            Some(dir) => {
                let dir = dir.display().to_string();
                self.cd(&[dir], out, err)
            }
            None => {
                let _ = writeln!(
                    err,
                    "z: no folder you've been to matches {}",
                    args.join(" ")
                );
                1
            }
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
            // `...` is `../..`, `....` is `../../..`.
            Some(dots) if dots.len() > 2 && dots.chars().all(|c| c == '.') => {
                let up = vec![".."; dots.len() - 1];
                PathBuf::from(up.join(std::path::MAIN_SEPARATOR_STR))
            }
            Some(dir) => PathBuf::from(dir),
        };
        let here = std::env::current_dir().ok();
        match std::env::set_current_dir(&target) {
            Ok(()) => {}
            Err(e) => {
                // Not a folder here: maybe one you've been to (as `z`).
                let name = target.display().to_string();
                let jump = (!name.contains(['/', '\\']) && args.len() == 1)
                    .then(|| self.jumps.matches(std::slice::from_ref(&name)))
                    .and_then(|m| m.into_iter().next());
                match jump {
                    Some(dir) if std::env::set_current_dir(&dir).is_ok() => {
                        let _ = writeln!(out, "\x1b[2m\u{2192} {}\x1b[0m", tilde(&dir));
                    }
                    _ => {
                        let _ = writeln!(err, "cd: {}: {e}", target.display());
                        return 1;
                    }
                }
            }
        }
        self.prev_dir = here;
        if let Ok(now) = std::env::current_dir() {
            self.jumps.visit(&now);
        }
        0
    }

    /// Run a file of commands; stops at `exit`.
    pub fn source(&mut self, file: &Path, err: &mut Vec<u8>) -> i32 {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                let _ = writeln!(err, "tacoshell: {}: {e}", file.display());
                return 1;
            }
        };
        self.run_lines(text.lines().map(String::from))
    }
}

impl Default for Shell {
    fn default() -> Self {
        Shell::new()
    }
}

/// A name that is plainly a folder to go to: `..`, `...`, or ends in a
/// slash.
fn auto_cd(name: &str) -> bool {
    (name.len() >= 2 && name.chars().all(|c| c == '.'))
        || (name.ends_with(['/', '\\']) && Path::new(name).is_dir())
}

/// Read up to and including a newline, a byte at a time so nothing after
/// it is used up.
fn read_one_line(mut from: impl Read, line: &mut String) -> usize {
    let mut bytes = Vec::new();
    let mut b = [0u8];
    while let Ok(1) = from.read(&mut b) {
        bytes.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
    }
    line.push_str(&String::from_utf8_lossy(&bytes));
    bytes.len()
}

/// `test` / `[`: files (`-e -f -d -s`), strings (`-z -n = !=`), numbers
/// (`-eq -ne -lt -le -gt -ge`), `!`, and `-a` / `-o` to combine.
fn test(args: &[String]) -> Result<bool, String> {
    if let Some(i) = args.iter().position(|a| a == "-o") {
        return Ok(test(&args[..i])? || test(&args[i + 1..])?);
    }
    if let Some(i) = args.iter().position(|a| a == "-a") {
        return Ok(test(&args[..i])? && test(&args[i + 1..])?);
    }
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let num = |s: &str| {
        s.trim()
            .parse::<i64>()
            .map_err(|_| format!("not a number: {s}"))
    };
    Ok(match a.as_slice() {
        [] => false,
        ["!", rest @ ..] => !test(&rest.iter().map(|s| s.to_string()).collect::<Vec<_>>())?,
        [s] => !s.is_empty(),
        [op, s] => {
            let p = Path::new(s);
            match *op {
                "-e" => p.exists(),
                "-f" => p.is_file(),
                "-d" => p.is_dir(),
                "-s" => p.metadata().is_ok_and(|m| m.len() > 0),
                "-z" => s.is_empty(),
                "-n" => !s.is_empty(),
                _ => return Err(format!("unknown test {op}")),
            }
        }
        [x, op, y] => match *op {
            "=" | "==" => x == y,
            "!=" => x != y,
            "-eq" => num(x)? == num(y)?,
            "-ne" => num(x)? != num(y)?,
            "-lt" => num(x)? < num(y)?,
            "-le" => num(x)? <= num(y)?,
            "-gt" => num(x)? > num(y)?,
            "-ge" => num(x)? >= num(y)?,
            _ => return Err(format!("unknown test {op}")),
        },
        _ => return Err("too many words; quote them or use -a / -o".into()),
    })
}

/// A function body written back out, for `functions NAME`.
fn show_script(script: &Script, indent: usize) -> String {
    let pad = "    ".repeat(indent);
    let mut out = String::new();
    let mut line = String::new();
    for (stmt, connector) in &script.items {
        match stmt {
            Stmt::Pipeline(p) => line.push_str(&show_pipeline(p)),
            Stmt::If {
                cond,
                then,
                otherwise,
            } => {
                line.push_str(&format!("if {}\n", show_inline(cond)));
                line.push_str(&show_script(then, indent + 1));
                if let Some(o) = otherwise {
                    line.push_str(&format!("{pad}else\n{}", show_script(o, indent + 1)));
                }
                line.push_str(&format!("{pad}end"));
            }
            Stmt::While { cond, body } => line.push_str(&format!(
                "while {}\n{}{pad}end",
                show_inline(cond),
                show_script(body, indent + 1)
            )),
            Stmt::For { var, items, body } => line.push_str(&format!(
                "for {var} in {}\n{}{pad}end",
                items.iter().map(show_word).collect::<Vec<_>>().join(" "),
                show_script(body, indent + 1)
            )),
            Stmt::Function { name, body } => line.push_str(&format!(
                "function {name}\n{}{pad}end",
                show_script(body, indent + 1)
            )),
        }
        match connector {
            Connector::And => line.push_str(" && "),
            Connector::Or => line.push_str(" || "),
            Connector::Then => {
                out.push_str(&format!("{pad}{line}\n"));
                line.clear();
            }
        }
    }
    if !line.is_empty() {
        out.push_str(&format!("{pad}{line}\n"));
    }
    out
}

fn show_inline(script: &Script) -> String {
    show_script(script, 0).trim_end().replace('\n', "; ")
}

fn show_pipeline(p: &Pipeline) -> String {
    let commands: Vec<String> = p
        .commands
        .iter()
        .map(|c| {
            let mut words: Vec<String> = c.words.iter().map(show_word).collect();
            for r in &c.redirects {
                words.push(match r {
                    Redirect::Out { fd, append, target } => format!(
                        "{}{}{}",
                        if *fd == 2 { "2" } else { "" },
                        if *append { ">>" } else { ">" },
                        show_word(target)
                    ),
                    Redirect::In(t) => format!("<{}", show_word(t)),
                    Redirect::ErrToOut => "2>&1".into(),
                });
            }
            words.join(" ")
        })
        .collect();
    format!(
        "{}{}",
        if p.negate { "not " } else { "" },
        commands.join(" | ")
    )
}

fn show_word(word: &Word) -> String {
    word.0
        .iter()
        .map(|part| match part {
            Part::Lit(s) => quote(s),
            Part::Wild(c) => c.to_string(),
            Part::Var(n) => format!("${{{n}}}"),
            Part::AllArgs { quoted: true } => "\"$argv\"".into(),
            Part::AllArgs { quoted: false } => "$argv".into(),
            Part::Status => "$?".into(),
            Part::Home => "~".into(),
            Part::Brace(choices) => format!(
                "{{{}}}",
                choices.iter().map(show_word).collect::<Vec<_>>().join(",")
            ),
            Part::Subst {
                script,
                quoted: true,
            } => format!("\"$({script})\""),
            Part::Subst {
                script,
                quoted: false,
            } => format!("$({script})"),
            Part::Arith(e) => format!("$(({e}))"),
        })
        .collect()
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
    let names_only = args.iter().any(|a| a == "-1");
    let mut paths: Vec<&str> = args
        .iter()
        .filter(|a| *a != "-a" && *a != "-1")
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
        let mut entries: Vec<std::fs::DirEntry> = entries
            .flatten()
            .filter(|e| all || !e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
        if names_only {
            for e in entries {
                let _ = writeln!(out, "{}", e.file_name().to_string_lossy());
            }
            continue;
        }
        let rows: Vec<[String; 3]> = entries
            .iter()
            .map(|e| {
                let (kind, size) = match e.file_type() {
                    Ok(t) if t.is_symlink() => ("LINK", 0),
                    Ok(t) if t.is_dir() => ("FOLDER", commands::folder_size(&e.path())),
                    _ => ("FILE", e.metadata().map_or(0, |m| m.len())),
                };
                [
                    kind.to_string(),
                    e.file_name().to_string_lossy().into_owned(),
                    commands::human_size(size),
                ]
            })
            .collect();
        let _ = out.write_all(commands::table(["TYPE", "NAME", "SIZE"], &rows, &[2]).as_bytes());
    }
    status
}

const HELP: &str = "\
tacoshell: a command shell for Windows

  a | b        pipe a's output into b
  a && b       run b if a worked;  a || b  run b if a failed;  not a / ! a
  a ; b        run both (a & b works too, as in cmd, and so does a new line)
  > f  >> f    send output to file f (>> appends); 2> f for errors; 2>&1
  < f          read input from file f
  'text'       taken as is;  \"text\"  expands $VAR, %VAR% and $(...)
  `x           a literal x (backslash is just a path separator)
  $VAR %VAR%   environment variables;  $?  the last exit code;  ~  home
  *.txt  ?     matching files;  **/*.rs  in every folder below too
  {a,b}        both: file.{txt,md} is file.txt file.md
  $(cmd)       cmd's output (a word per line unless quoted)
  $((1 + 2))   arithmetic;  math 2 ** 10  prints it
  NAME=v cmd   run cmd with NAME set;  !! the last line;  !$ its last word
  # text       a comment

Blocks (on one line with ; or over several lines):
  if test -f x; echo yes; else if ...; else; echo no; end
  for f in *.txt; echo $f; end          while test $n -lt 5; ...; end
  function greet; echo hi $argv ($1, $2, $#); end     break  continue  return
  test / [ ]   -e -f -d -s file, -z -n string, = !=, -eq -ne -lt -le -gt -ge
  read NAME    a line of input into NAME

Built-ins:
  cd [dir|-|...]  change folder (no dir: home; -: the previous one; D: works;
               a name that isn't here goes to the best match you've visited)
  ..  ...  dir/   typed alone, go there
  z words      jump to the folder you use most that matches;  z  lists them
  ls [-a] [-1] list a folder: type, name, size (-1: names only)
  pwd          show the current folder
  echo [-n]    print words          clear   clear the screen (or cls)
  set NAME=value  (or NAME=value)   unset NAME    env    list variables
  alias name='cmd args'   unalias name   which / type name   history [word|-c]
  functions [name] [-e name]   source file [args]   exit [code]
  pushd dir / popd / dirs   a stack of folders to go back to

Files and text (they read files, or a pipe or < file when given none):
  cat  head [-n N]  tail [-n N]  wc [-l -w -c]  sort [-r -n -f -u]  uniq [-c]
  grep [-i -v -n -c -F] PATTERN   tee [-a] file
  cut -f LIST [-d D] | -c LIST    tr SET1 SET2 | tr -d SET   seq [FIRST [STEP]] LAST
  mkdir [-p]  touch  cp [-r]  mv  rm [-r -f]  basename  dirname  realpath
  find [path] [-name GLOB] [-iname GLOB] [-type f|d] [-maxdepth N]
  du [path...]  sizes        df   free space on each drive
  ps   running programs      kill [-9] PID...
  date [+FORMAT]   time COMMAND (how long it took)   sleep secs
  open file-or-url (in its usual app)
In the terminal, cat, head, tail and grep show their text in a box, so it
stands apart from what you typed (piped or saved, it's left as is).

Pictures:
  img [-k | -s | -b] [-w columns] picture   kitty graphics, sixel, or
               colored blocks (works everywhere); picks kitty in terminals
               known to show it. TACOSHELL_IMG=kitty|sixel|blocks overrides.

The prompt shows the folder, the git branch, and how long a slow command
took. Make your own with TACOSHELL_PROMPT, e.g.
  set TACOSHELL_PROMPT='{cyan}{user}@{host}{reset} {cwd}{git} {arrow} '
({dir} {branch} {status} {took} {time} {nl} and {red} {green} {yellow} {blue}
{magenta} {gray} {bold} {dim} {reset} work too), or with a function named
prompt, whose output becomes the prompt.

cmd.exe's own commands (dir, copy, del, type, mklink, start, ...) run through
cmd. %APPDATA%\\tacoshell\\init.tsh runs at startup, for aliases and such.

Keys: Tab completes; as you type, commands turn green (or red if there's no
such command) and a gray suggestion from history appears: Right or End takes
it. Ctrl+R searches history. Up/Down history, Ctrl+Left/Right by word,
Ctrl+W/U/K delete a word / to start / to end, Esc clears the line, Ctrl+L
clears the screen, Ctrl+C cancels, Ctrl+D on an empty line exits.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_explorer() {
        use commands::human_size;
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.50 KB");
        assert_eq!(human_size(2_330_000_000_000), "2.12 TB");
    }

    #[test]
    fn lists_type_name_and_size() {
        let dir = std::env::temp_dir().join(format!("tacoshell-ls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("weluvbanana/deep")).unwrap();
        std::fs::write(dir.join("weluvbanana/deep/a.bin"), vec![0u8; 2048]).unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(ls(&[dir.display().to_string()], &mut out, &mut err), 0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "TYPE   | NAME        |    SIZE\n\
             FILE   | notes.txt   |     5 B\n\
             FOLDER | weluvbanana | 2.00 KB\n"
        );
        let mut out = Vec::new();
        ls(
            &["-1".into(), dir.display().to_string()],
            &mut out,
            &mut err,
        );
        assert_eq!(String::from_utf8(out).unwrap(), "notes.txt\nweluvbanana\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_expansion() {
        let h = vec!["git commit -m x".to_string()];
        assert_eq!(
            expand_history("sudo !!", &h).as_deref(),
            Some("sudo git commit -m x")
        );
        assert_eq!(expand_history("echo !$", &h).as_deref(), Some("echo x"));
        assert_eq!(expand_history("echo '!!'", &h), None);
        assert_eq!(expand_history("test 1 != 2", &h), None);
    }

    #[test]
    fn tests() {
        let t = |s: &str| {
            test(
                &s.split(' ')
                    .filter(|w| !w.is_empty())
                    .map(String::from)
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(t("a = a"), Ok(true));
        assert_eq!(t("! a = a"), Ok(false));
        assert_eq!(t("3 -lt 10"), Ok(true));
        assert_eq!(t("-z x -o 1 -eq 1"), Ok(true));
        assert_eq!(t("-n x -a -d /no/such/dir"), Ok(false));
        assert_eq!(t(""), Ok(false));
        assert!(t("a -lt 3").is_err());
    }

    #[test]
    fn shows_functions() {
        let s = parse::parse(
            "function f; if test -f $1; echo \"yes $1\" | sort; else; echo no; end; end",
            &HashMap::new(),
        )
        .unwrap();
        let Stmt::Function { body, .. } = &s.items[0].0 else {
            panic!()
        };
        let shown = show_script(body, 1);
        assert_eq!(
            shown,
            "    if test -f ${1}\n        echo 'yes '${1} | sort\n    else\n        echo no\n    end\n"
        );
        // What's shown parses back the same.
        let again = parse::parse(&format!("function f\n{shown}end"), &HashMap::new()).unwrap();
        assert_eq!(again, s);
    }
}
