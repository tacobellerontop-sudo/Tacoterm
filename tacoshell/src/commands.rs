//! The everyday commands other shells give you (cat, grep, head, rm, cp…),
//! built in so they work the same on every Windows machine. They read
//! files, or their input (a pipe, `< file`, or the console) when given none.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const NAMES: &[&str] = &[
    "basename", "cat", "cp", "cut", "date", "df", "dirname", "du", "find", "grep", "head", "img",
    "kill", "mkdir", "mv", "open", "ps", "realpath", "rm", "seq", "sleep", "sort", "tail", "tee",
    "touch", "tr", "uniq", "wc",
];

/// Commands whose output is text to read; in a terminal it goes in a box,
/// so it stands apart from what you typed.
const BOXED: &[&str] = &["cat", "grep", "head", "tail"];

/// Where a command's input comes from.
pub enum Source {
    Pipe(std::io::PipeReader),
    File(File),
}

impl Source {
    pub fn stdio(self) -> std::process::Stdio {
        match self {
            Source::Pipe(p) => p.into(),
            Source::File(f) => f.into(),
        }
    }
}

/// A built-in's input, read only if the command wants it.
pub struct Input(pub Option<Source>);

impl Input {
    pub fn read_all(&mut self) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        match self.0.take() {
            Some(Source::Pipe(mut p)) => p.read_to_end(&mut buf)?,
            Some(Source::File(mut f)) => f.read_to_end(&mut buf)?,
            None => std::io::stdin().lock().read_to_end(&mut buf)?,
        };
        Ok(buf)
    }
}

/// Command-line flags: single letters (`-rf`), `-n 5` style values, and the
/// rest as operands. `--` ends flags.
struct Args {
    flags: Vec<char>,
    values: Vec<(char, String)>,
    operands: Vec<String>,
}

impl Args {
    fn parse(args: &[String], with_value: &[char]) -> Result<Args, String> {
        let mut out = Args {
            flags: Vec::new(),
            values: Vec::new(),
            operands: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg == "--" {
                out.operands.extend(it.cloned());
                break;
            }
            let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else {
                out.operands.push(arg.clone());
                continue;
            };
            // `-5` means `-n 5` for head and tail.
            if letters.chars().all(|c| c.is_ascii_digit()) && with_value.contains(&'n') {
                out.values.push(('n', letters.to_string()));
                continue;
            }
            for (i, c) in letters.char_indices() {
                if with_value.contains(&c) {
                    let rest = &letters[i + c.len_utf8()..];
                    let value = if rest.is_empty() {
                        it.next().cloned().ok_or(format!("-{c} needs a value"))?
                    } else {
                        rest.to_string()
                    };
                    out.values.push((c, value));
                    break;
                }
                out.flags.push(c);
            }
        }
        Ok(out)
    }

    fn has(&self, c: char) -> bool {
        self.flags.contains(&c)
    }

    fn value(&self, c: char) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| *k == c)
            .map(|(_, v)| v.as_str())
    }

    fn count(&self, default: usize) -> Result<usize, String> {
        match self.value('n') {
            Some(v) => v.parse().map_err(|_| format!("not a number: {v}")),
            None => Ok(default),
        }
    }
}

/// Run built-in `name`; `None` if it isn't one of these. `to_terminal`
/// says the output goes straight to the screen (rather than a pipe or file),
/// where text output is drawn in a box.
pub fn run(
    name: &str,
    args: &[String],
    input: &mut Input,
    to_terminal: bool,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> Option<i32> {
    let status = run_command(name, args, input, out, err)?;
    if to_terminal && BOXED.contains(&name) && !out.is_empty() {
        let title = match name {
            "grep" => format!(
                "grep {}",
                args.iter()
                    .find(|a| !a.starts_with('-'))
                    .map_or("", |a| a.as_str())
            ),
            _ => {
                let files: Vec<&str> = args
                    .iter()
                    .filter(|a| !a.starts_with('-') && a.parse::<usize>().is_err())
                    .map(String::as_str)
                    .collect();
                if files.is_empty() {
                    name.to_string()
                } else {
                    files.join(", ")
                }
            }
        };
        let text = String::from_utf8_lossy(out).into_owned();
        *out = boxed(&title, &text, crate::term::columns()).into_bytes();
    }
    Some(status)
}

fn run_command(
    name: &str,
    args: &[String],
    input: &mut Input,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> Option<i32> {
    // These read their arguments themselves (`find -name`, `seq -5 5`).
    match name {
        "find" => return Some(find(args, out, err)),
        "seq" => return Some(seq(args, out, err)),
        _ => {}
    }
    let with_value: &[char] = match name {
        "head" | "tail" => &['n'],
        "img" => &['w'],
        "cut" => &['d', 'f', 'c'],
        _ => &[],
    };
    let args = match Args::parse(args, with_value) {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(err, "{name}: {e}");
            return Some(2);
        }
    };
    let result = match name {
        "cat" => each_input(&args.operands, input, err, |_, data| {
            out.extend_from_slice(&data);
            Ok(())
        }),
        "head" | "tail" => args.count(10).and_then(|n| {
            each_input(&args.operands, input, err, |_, data| {
                let text = String::from_utf8_lossy(&data);
                let lines: Vec<&str> = text.lines().collect();
                let pick = if name == "head" {
                    &lines[..n.min(lines.len())]
                } else {
                    &lines[lines.len().saturating_sub(n)..]
                };
                for line in pick {
                    let _ = writeln!(out, "{line}");
                }
                Ok(())
            })
        }),
        "wc" => {
            let all = !(args.has('l') || args.has('w') || args.has('c'));
            let many = args.operands.len() > 1;
            let mut total = [0usize; 3];
            let r = each_input(&args.operands, input, err, |label, data| {
                let text = String::from_utf8_lossy(&data);
                let counts = [
                    text.lines().count(),
                    text.split_whitespace().count(),
                    data.len(),
                ];
                for (t, c) in total.iter_mut().zip(counts) {
                    *t += c;
                }
                write_counts(out, &counts, all, &args, if many { label } else { "" });
                Ok(())
            });
            if many {
                write_counts(out, &total, all, &args, "total");
            }
            r
        }
        "grep" => return Some(grep(&args, input, out, err)),
        "sort" => each_input(&args.operands, input, err, |_, data| {
            out.extend_from_slice(&data);
            Ok(())
        })
        .map(|()| {
            let text = String::from_utf8_lossy(out).into_owned();
            out.clear();
            let mut lines: Vec<&str> = text.lines().collect();
            if args.has('n') {
                let num = |s: &str| s.trim().parse::<f64>().unwrap_or(f64::NEG_INFINITY);
                lines.sort_by(|a, b| num(a).total_cmp(&num(b)).then_with(|| a.cmp(b)));
            } else if args.has('f') {
                lines.sort_by_key(|l| l.to_lowercase());
            } else {
                lines.sort();
            }
            if args.has('r') {
                lines.reverse();
            }
            if args.has('u') {
                lines.dedup();
            }
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
        }),
        "uniq" => each_input(&args.operands, input, err, |_, data| {
            let text = String::from_utf8_lossy(&data);
            let mut runs: Vec<(usize, &str)> = Vec::new();
            for line in text.lines() {
                match runs.last_mut() {
                    Some((n, last)) if *last == line => *n += 1,
                    _ => runs.push((1, line)),
                }
            }
            for (n, line) in runs {
                let _ = if args.has('c') {
                    writeln!(out, "{n:7} {line}")
                } else {
                    writeln!(out, "{line}")
                };
            }
            Ok(())
        }),
        "tee" => input
            .read_all()
            .map_err(|e| e.to_string())
            .and_then(|data| {
                out.extend_from_slice(&data);
                for file in &args.operands {
                    let opened = fs::OpenOptions::new()
                        .create(true)
                        .append(args.has('a'))
                        .write(true)
                        .truncate(!args.has('a'))
                        .open(file);
                    opened
                        .and_then(|mut f| f.write_all(&data))
                        .map_err(|e| format!("{file}: {e}"))?;
                }
                Ok(())
            }),
        "mkdir" => each_operand(&args.operands, err, |p| {
            if args.has('p') {
                fs::create_dir_all(p)
            } else {
                fs::create_dir(p)
            }
        }),
        "rm" => each_operand(&args.operands, err, |p| {
            let path = Path::new(p);
            match path.symlink_metadata() {
                Err(_) if args.has('f') => Ok(()),
                Ok(m) if m.is_dir() && !(args.has('r') || args.has('R')) => {
                    Err(std::io::Error::other("is a folder (use rm -r)"))
                }
                Ok(m) if m.is_dir() => fs::remove_dir_all(path),
                _ => fs::remove_file(path),
            }
        }),
        "touch" => each_operand(&args.operands, err, |p| {
            let f = fs::OpenOptions::new().create(true).append(true).open(p)?;
            f.set_modified(std::time::SystemTime::now())
        }),
        "cp" | "mv" => copy_or_move(name == "mv", &args, err),
        "sleep" => match args
            .operands
            .first()
            .and_then(|s| s.trim_end_matches('s').parse::<f64>().ok())
        {
            Some(secs) if secs >= 0.0 => {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs));
                Ok(())
            }
            _ => Err("sleep SECONDS".to_string()),
        },
        "basename" | "dirname" | "realpath" => {
            for p in &args.operands {
                let path = Path::new(p);
                let shown = match name {
                    "basename" => path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    "dirname" => match path.parent().map(|d| d.display().to_string()) {
                        Some(d) if !d.is_empty() => d,
                        _ => ".".to_string(),
                    },
                    _ => match fs::canonicalize(path) {
                        Ok(full) => strip_verbatim(&full),
                        Err(e) => {
                            let _ = writeln!(err, "realpath: {p}: {e}");
                            continue;
                        }
                    },
                };
                let _ = writeln!(out, "{shown}");
            }
            Ok(())
        }
        "open" => each_operand(&args.operands, err, open),
        "img" => {
            use crate::img::Mode;
            let mode = [('k', Mode::Kitty), ('s', Mode::Sixel), ('b', Mode::Blocks)]
                .into_iter()
                .find(|(flag, _)| args.has(*flag))
                .map(|(_, mode)| mode);
            return Some(crate::img::run(
                &args.operands,
                mode,
                args.value('w'),
                out,
                err,
            ));
        }
        "date" => date(args.operands.first().map(String::as_str), out),
        "du" => {
            let mut rows = Vec::new();
            let mut total = 0;
            let targets: Vec<PathBuf> = if args.operands.is_empty() {
                match fs::read_dir(".") {
                    Ok(entries) => {
                        let mut v: Vec<PathBuf> = entries
                            .flatten()
                            .map(|e| PathBuf::from(e.file_name()))
                            .collect();
                        v.sort_by_key(|p| p.display().to_string().to_lowercase());
                        v
                    }
                    Err(e) => return Some(fail(name, e.to_string(), err)),
                }
            } else {
                args.operands.iter().map(PathBuf::from).collect()
            };
            for path in targets {
                let size = match path.symlink_metadata() {
                    Ok(m) if m.is_dir() => folder_size(&path),
                    Ok(m) => m.len(),
                    Err(e) => {
                        let _ = writeln!(err, "{}: {e}", path.display());
                        continue;
                    }
                };
                total += size;
                rows.push([human_size(size), path.display().to_string()]);
            }
            if rows.len() > 1 {
                rows.push([human_size(total), "(total)".to_string()]);
            }
            let _ = out.write_all(table(["SIZE", "NAME"], &rows, &[0]).as_bytes());
            Ok(())
        }
        "df" => {
            let rows: Vec<[String; 5]> = disks()
                .into_iter()
                .map(|(name, total, free)| {
                    let used = total.saturating_sub(free);
                    let pct = (used * 100).checked_div(total).unwrap_or(0);
                    [
                        name,
                        human_size(total),
                        human_size(used),
                        human_size(free),
                        format!("{pct}%"),
                    ]
                })
                .collect();
            let _ = out.write_all(
                table(
                    ["DRIVE", "SIZE", "USED", "FREE", "USE%"],
                    &rows,
                    &[1, 2, 3, 4],
                )
                .as_bytes(),
            );
            Ok(())
        }
        "ps" => {
            let program = if cfg!(windows) { "tasklist" } else { "ps" };
            let mut c = std::process::Command::new(program);
            if !cfg!(windows) {
                c.args(&args.operands);
                if args.operands.is_empty() {
                    c.arg("-e");
                }
            }
            match c.output() {
                Ok(o) => {
                    out.extend_from_slice(&o.stdout);
                    err.extend_from_slice(&o.stderr);
                    Ok(())
                }
                Err(e) => Err(format!("{program}: {e}")),
            }
        }
        "kill" => {
            let force = args.has('9') || args.has('f');
            each_operand(&args.operands, err, |pid| kill(pid, force))
        }
        "cut" => {
            let delim = args
                .value('d')
                .map(|d| d.chars().next().unwrap_or('\t'))
                .unwrap_or('\t');
            let (list, by_char) = match (args.value('f'), args.value('c')) {
                (Some(f), _) => (f, false),
                (None, Some(c)) => (c, true),
                _ => {
                    return Some(fail(
                        name,
                        "cut -f LIST [-d DELIM]  or  cut -c LIST".into(),
                        err,
                    ))
                }
            };
            match parse_list(list) {
                Err(e) => Err(e),
                Ok(ranges) => each_input(&args.operands, input, err, |_, data| {
                    let text = String::from_utf8_lossy(&data);
                    let wanted =
                        |i: usize| ranges.iter().any(|(a, b)| (*a..=*b).contains(&(i + 1)));
                    for line in text.lines() {
                        let picked: String = if by_char {
                            line.chars()
                                .enumerate()
                                .filter(|(i, _)| wanted(*i))
                                .map(|(_, c)| c)
                                .collect()
                        } else {
                            let fields: Vec<&str> = line
                                .split(delim)
                                .enumerate()
                                .filter(|(i, _)| wanted(*i))
                                .map(|(_, f)| f)
                                .collect();
                            fields.join(&delim.to_string())
                        };
                        let _ = writeln!(out, "{picked}");
                    }
                    Ok(())
                }),
            }
        }
        "tr" => {
            let from = args
                .operands
                .first()
                .map(|s| expand_set(s))
                .unwrap_or_default();
            let to = args
                .operands
                .get(1)
                .map(|s| expand_set(s))
                .unwrap_or_default();
            if from.is_empty() || (!args.has('d') && to.is_empty()) {
                return Some(fail(
                    name,
                    "tr SET1 SET2  or  tr -d SET1  (sets like a-z)".into(),
                    err,
                ));
            }
            input.read_all().map_err(|e| e.to_string()).map(|data| {
                let text = String::from_utf8_lossy(&data);
                let mapped: String = text
                    .chars()
                    .filter_map(|c| match from.iter().position(|&f| f == c) {
                        None => Some(c),
                        Some(_) if args.has('d') => None,
                        // A shorter SET2 repeats its last character.
                        Some(i) => Some(*to.get(i).unwrap_or(to.last().unwrap())),
                    })
                    .collect();
                out.extend_from_slice(mapped.as_bytes());
            })
        }
        _ => return None,
    };
    Some(match result {
        Ok(()) if err.is_empty() => 0,
        Ok(()) => 1,
        Err(e) => {
            let _ = writeln!(err, "{name}: {e}");
            1
        }
    })
}

fn fail(name: &str, message: String, err: &mut Vec<u8>) -> i32 {
    let _ = writeln!(err, "{name}: {message}");
    2
}

/// Draw text in a rounded box, wrapped to fit `cols`, with a title on top.
pub fn boxed(title: &str, text: &str, cols: usize) -> String {
    use crate::editor::str_width;
    let max = cols.saturating_sub(4).max(10);
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let clean: String = raw
            .replace('\t', "    ")
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let mut line = String::new();
        for c in clean.chars() {
            if str_width(&line) + str_width(&c.to_string()) > max {
                lines.push(std::mem::take(&mut line));
            }
            line.push(c);
        }
        lines.push(line);
    }
    let mut title: String = title.chars().filter(|c| !c.is_control()).collect();
    while str_width(&title) + 2 > max {
        title.pop();
    }
    let label = format!(" {title} ");
    let width = lines
        .iter()
        .map(|l| str_width(l))
        .max()
        .unwrap_or(0)
        .max(str_width(&label))
        .min(max);
    let (dim, reset) = ("\x1b[2m", "\x1b[0m");
    let mut s = format!(
        "{dim}\u{256d}\u{2500}{reset}{label}{dim}{}\u{256e}{reset}\n",
        "\u{2500}".repeat(width + 1 - str_width(&label))
    );
    for line in &lines {
        let pad = " ".repeat(width - str_width(line));
        s.push_str(&format!(
            "{dim}\u{2502}{reset} {line}{pad} {dim}\u{2502}{reset}\n"
        ));
    }
    s.push_str(&format!(
        "{dim}\u{2570}{}\u{256f}{reset}\n",
        "\u{2500}".repeat(width + 2)
    ));
    s
}

/// Everything under a folder, added up. Links aren't followed, and what
/// can't be read counts as nothing.
pub fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_symlink() => 0,
            Ok(t) if t.is_dir() => folder_size(&e.path()),
            _ => e.metadata().map_or(0, |m| m.len()),
        })
        .sum()
}

/// `512 B`, `1.50 KB`, `2.12 TB` (1 KB = 1024 bytes, like Explorer).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Columns padded to line up and split by ` | `; the columns in `right`
/// are right-aligned (numbers and sizes).
pub fn table<const N: usize>(header: [&str; N], rows: &[[String; N]], right: &[usize]) -> String {
    use crate::editor::str_width;
    let mut widths = header.map(str_width);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(str_width(cell));
        }
    }
    let line = |cells: [&str; N]| {
        let shown: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let pad = " ".repeat(widths[i] - str_width(cell));
                if right.contains(&i) {
                    format!("{pad}{cell}")
                } else if i + 1 == N {
                    cell.to_string()
                } else {
                    format!("{cell}{pad}")
                }
            })
            .collect();
        shown.join(" | ") + "\n"
    };
    let mut s = line(header);
    for row in rows {
        s.push_str(&line(std::array::from_fn(|i| row[i].as_str())));
    }
    s
}

/// `date` or `date +FORMAT` (strftime: `+%Y-%m-%d %H:%M`).
fn date(format: Option<&str>, out: &mut Vec<u8>) -> Result<(), String> {
    use chrono::format::{Item, StrftimeItems};
    let format = match format {
        Some(f) => f
            .strip_prefix('+')
            .ok_or("date [+FORMAT], like date +%Y-%m-%d")?,
        None => "%a %d %b %Y %H:%M:%S %:z",
    };
    if StrftimeItems::new(format).any(|i| matches!(i, Item::Error)) {
        return Err(format!("bad format: {format}"));
    }
    let _ = writeln!(out, "{}", chrono::Local::now().format(format));
    Ok(())
}

/// `*` and `?` wildcards.
fn wildcard(pattern: &[char], name: &[char]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            wildcard(&pattern[1..], name) || (!name.is_empty() && wildcard(pattern, &name[1..]))
        }
        (Some('?'), Some(_)) => wildcard(&pattern[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => wildcard(&pattern[1..], &name[1..]),
        _ => false,
    }
}

/// `find [PATH...] [-name GLOB] [-iname GLOB] [-type f|d] [-maxdepth N]`
fn find(args: &[String], out: &mut Vec<u8>, err: &mut Vec<u8>) -> i32 {
    let mut roots = Vec::new();
    let mut name: Option<(Vec<char>, bool)> = None;
    let mut kind: Option<char> = None;
    let mut max_depth = usize::MAX;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        let parsed = match arg.as_str() {
            "-name" | "-iname" => value().map(|v| {
                let ci = arg == "-iname" || cfg!(windows);
                let v = if ci { v.to_lowercase() } else { v };
                name = Some((v.chars().collect(), ci));
            }),
            "-type" => value().and_then(|v| match v.as_str() {
                "f" | "d" => {
                    kind = v.chars().next();
                    Ok(())
                }
                _ => Err("-type takes f (files) or d (folders)".to_string()),
            }),
            "-maxdepth" => value().and_then(|v| {
                v.parse()
                    .map(|n| max_depth = n)
                    .map_err(|_| format!("not a number: {v}"))
            }),
            a if a.starts_with('-') => Err(format!("unknown option {a}")),
            a => {
                roots.push(PathBuf::from(a));
                Ok(())
            }
        };
        if let Err(e) = parsed {
            return fail("find", e, err);
        }
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }
    fn walk(
        path: &Path,
        depth: usize,
        max: usize,
        visit: &mut dyn FnMut(&Path, bool),
        err: &mut Vec<u8>,
    ) {
        let is_dir = path.symlink_metadata().is_ok_and(|m| m.is_dir());
        visit(path, is_dir);
        if !is_dir || depth >= max {
            return;
        }
        match fs::read_dir(path) {
            Ok(entries) => {
                let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                entries.sort();
                for e in entries {
                    walk(&e, depth + 1, max, visit, err);
                }
            }
            Err(e) => {
                let _ = writeln!(err, "find: {}: {e}", path.display());
            }
        }
    }
    let mut visit = |path: &Path, is_dir: bool| {
        if kind == Some('f') && is_dir || kind == Some('d') && !is_dir {
            return;
        }
        if let Some((pattern, ci)) = &name {
            let file = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let file = if *ci { file.to_lowercase() } else { file };
            if !wildcard(pattern, &file.chars().collect::<Vec<_>>()) {
                return;
            }
        }
        let _ = writeln!(out, "{}", path.display());
    };
    for root in &roots {
        if root.symlink_metadata().is_err() {
            let _ = writeln!(err, "find: {}: not found", root.display());
            continue;
        }
        walk(root, 0, max_depth, &mut visit, err);
    }
    if err.is_empty() {
        0
    } else {
        1
    }
}

/// `seq LAST`, `seq FIRST LAST`, `seq FIRST STEP LAST`
fn seq(args: &[String], out: &mut Vec<u8>, err: &mut Vec<u8>) -> i32 {
    let nums: Result<Vec<i64>, _> = args.iter().map(|a| a.parse::<i64>()).collect();
    let (first, step, last) = match nums.as_deref() {
        Ok([last]) => (1, 1, *last),
        Ok([first, last]) => (*first, 1, *last),
        Ok([first, step, last]) if *step != 0 => (*first, *step, *last),
        _ => return fail("seq", "seq [FIRST [STEP]] LAST, whole numbers".into(), err),
    };
    let mut n = first;
    while (step > 0 && n <= last) || (step < 0 && n >= last) {
        let _ = writeln!(out, "{n}");
        n += step;
    }
    0
}

/// `1,3,5-7` → ranges (1-based, inclusive). `-3` and `5-` work too.
fn parse_list(list: &str) -> Result<Vec<(usize, usize)>, String> {
    list.split(',')
        .map(|part| {
            let num = |s: &str, default: usize| {
                if s.is_empty() {
                    Ok(default)
                } else {
                    s.parse::<usize>().map_err(|_| format!("bad list: {list}"))
                }
            };
            match part.split_once('-') {
                Some((a, b)) => Ok((num(a, 1)?, num(b, usize::MAX)?)),
                None => {
                    let n = num(part, 0)?;
                    Ok((n, n))
                }
            }
        })
        .collect()
}

/// `a-z0-9_` → every character in it.
fn expand_set(set: &str) -> Vec<char> {
    let chars: Vec<char> = set.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i] <= chars[i + 2] {
            out.extend(chars[i]..=chars[i + 2]);
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn kill(pid: &str, force: bool) -> std::io::Result<()> {
    let pid: u32 = pid
        .parse()
        .map_err(|_| std::io::Error::other("not a process id"))?;
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string()]);
        if force {
            c.arg("/F");
        }
        let o = c.output()?;
        if o.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ))
        }
    }
    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        if unsafe { libc::kill(pid as libc::pid_t, signal) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Every drive (Windows) or the root file system: name, size, free bytes.
fn disks() -> Vec<(String, u64, u64)> {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives};
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .filter_map(|i| {
                let root = format!("{}:\\", (b'A' + i) as char);
                let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
                unsafe {
                    GetDiskFreeSpaceExW(
                        &HSTRING::from(root.as_str()),
                        Some(&mut avail),
                        Some(&mut total),
                        Some(&mut free),
                    )
                }
                .ok()?;
                Some((root, total, avail))
            })
            .collect()
    }
    #[cfg(unix)]
    {
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c"/".as_ptr(), &mut st) } != 0 {
            return Vec::new();
        }
        let block = st.f_frsize as u64;
        vec![(
            "/".to_string(),
            st.f_blocks as u64 * block,
            st.f_bavail as u64 * block,
        )]
    }
}

fn write_counts(out: &mut Vec<u8>, counts: &[usize; 3], all: bool, args: &Args, label: &str) {
    let shown: Vec<usize> = ['l', 'w', 'c']
        .iter()
        .zip(counts)
        .filter(|(flag, _)| all || args.has(**flag))
        .map(|(_, n)| *n)
        .collect();
    // One number alone is just the number; several line up in columns.
    let fields: Vec<String> = match shown.as_slice() {
        [n] => vec![n.to_string()],
        _ => shown.iter().map(|n| format!("{n:7}")).collect(),
    };
    let line = format!("{} {label}", fields.join(" "));
    let _ = writeln!(out, "{}", line.trim_end());
}

/// Call `f` with each file's bytes, or with the input when there are no
/// files (or for `-`). Unreadable files are reported and skipped.
fn each_input(
    files: &[String],
    input: &mut Input,
    err: &mut Vec<u8>,
    mut f: impl FnMut(&str, Vec<u8>) -> Result<(), String>,
) -> Result<(), String> {
    if files.is_empty() {
        return f("", input.read_all().map_err(|e| e.to_string())?);
    }
    for file in files {
        let data = if file == "-" {
            input.read_all()
        } else {
            fs::read(file)
        };
        match data {
            Ok(data) => f(file, data)?,
            Err(e) => {
                let _ = writeln!(err, "{file}: {e}");
            }
        }
    }
    Ok(())
}

fn each_operand(
    operands: &[String],
    err: &mut Vec<u8>,
    mut f: impl FnMut(&str) -> std::io::Result<()>,
) -> Result<(), String> {
    if operands.is_empty() {
        return Err("which file?".to_string());
    }
    for p in operands {
        if let Err(e) = f(p) {
            let _ = writeln!(err, "{p}: {e}");
        }
    }
    Ok(())
}

fn grep(args: &Args, input: &mut Input, out: &mut Vec<u8>, err: &mut Vec<u8>) -> i32 {
    let Some((pattern, files)) = args.operands.split_first() else {
        let _ = writeln!(err, "grep: grep [-i -v -n -c -F] PATTERN [FILE...]");
        return 2;
    };
    let source = if args.has('F') {
        regex::escape(pattern)
    } else {
        pattern.clone()
    };
    let re = match regex::RegexBuilder::new(&source)
        .case_insensitive(args.has('i'))
        .build()
    {
        Ok(re) => re,
        Err(e) => {
            let _ = writeln!(err, "grep: {e}");
            return 2;
        }
    };
    let mut found = false;
    let many = files.len() > 1;
    let r = each_input(files, input, err, |label, data| {
        let text = String::from_utf8_lossy(&data);
        let mut count = 0;
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) == args.has('v') {
                continue;
            }
            count += 1;
            if args.has('c') {
                continue;
            }
            let prefix = if many {
                format!("{label}:")
            } else {
                String::new()
            };
            let number = if args.has('n') {
                format!("{}:", i + 1)
            } else {
                String::new()
            };
            let _ = writeln!(out, "{prefix}{number}{line}");
        }
        if args.has('c') {
            let _ = writeln!(
                out,
                "{}{count}",
                if many {
                    format!("{label}:")
                } else {
                    String::new()
                }
            );
        }
        found |= count > 0;
        Ok(())
    });
    match r {
        Err(e) => {
            let _ = writeln!(err, "grep: {e}");
            2
        }
        Ok(()) if !err.is_empty() => 2,
        Ok(()) => (!found) as i32,
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

fn copy_or_move(is_move: bool, args: &Args, err: &mut Vec<u8>) -> Result<(), String> {
    let usage = if is_move {
        "mv FROM... TO"
    } else {
        "cp [-r] FROM... TO"
    };
    let Some((dest, sources)) = args.operands.split_last().filter(|(_, s)| !s.is_empty()) else {
        return Err(usage.to_string());
    };
    let dest = Path::new(dest);
    if sources.len() > 1 && !dest.is_dir() {
        return Err(format!("{} isn't a folder", dest.display()));
    }
    for src in sources {
        let from = Path::new(src);
        let to: PathBuf = if dest.is_dir() {
            match from.file_name() {
                Some(name) => dest.join(name),
                None => dest.to_path_buf(),
            }
        } else {
            dest.to_path_buf()
        };
        let result = if is_move {
            fs::rename(from, &to).or_else(|_| copy_tree(from, &to).and_then(|()| remove(from)))
        } else if from.is_dir() && !(args.has('r') || args.has('R')) {
            Err(std::io::Error::other("is a folder (use cp -r)"))
        } else {
            copy_tree(from, &to)
        };
        if let Err(e) = result {
            let _ = writeln!(err, "{src}: {e}");
        }
    }
    Ok(())
}

fn remove(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// `canonicalize` gives `\\?\C:\…` on Windows; show `C:\…`.
fn strip_verbatim(path: &Path) -> String {
    let s = path.display().to_string();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").unwrap_or(&s).to_string(),
    }
}

/// Open a file, folder or URL in its usual app.
fn open(target: &str) -> std::io::Result<()> {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(target);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with(name: &str, args: &[&str], stdin: &str) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(stdin.as_bytes()).unwrap();
        drop(writer);
        let mut input = Input(Some(Source::Pipe(reader)));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(name, &args, &mut input, false, &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn text_commands() {
        let text = "b\na\nc\na\na\n";
        assert_eq!(run_with("head", &["-2"], text).1, "b\na\n");
        assert_eq!(run_with("tail", &["-n", "2"], text).1, "a\na\n");
        assert_eq!(run_with("sort", &["-u"], text).1, "a\nb\nc\n");
        assert_eq!(run_with("sort", &["-rn"], "2\n10\n1\n").1, "10\n2\n1\n");
        assert_eq!(
            run_with("uniq", &["-c"], text).1,
            "      1 b\n      1 a\n      1 c\n      2 a\n"
        );
        assert_eq!(run_with("wc", &["-l"], text).1, "5\n");
        assert_eq!(run_with("wc", &[], "a b\n").1, "      1       2       4\n");
        assert_eq!(run_with("cat", &[], "x\r\ny").1, "x\r\ny");
        assert_eq!(run_with("basename", &["a/b/c.txt"], "").1, "c.txt\n");
        assert_eq!(run_with("dirname", &["c.txt"], "").1, ".\n");
    }

    #[test]
    fn more_text_commands() {
        assert_eq!(run_with("seq", &["3"], "").1, "1\n2\n3\n");
        assert_eq!(run_with("seq", &["5", "-2", "1"], "").1, "5\n3\n1\n");
        assert_eq!(
            run_with("cut", &["-d", ",", "-f", "1,3"], "a,b,c\nd,e,f\n").1,
            "a,c\nd,f\n"
        );
        assert_eq!(run_with("cut", &["-c", "2-3"], "hello\n").1, "el\n");
        assert_eq!(run_with("tr", &["a-z", "A-Z"], "taco\n").1, "TACO\n");
        assert_eq!(
            run_with("tr", &["-d", "aeiou"], "taco shell\n").1,
            "tc shll\n"
        );
        assert_eq!(run_with("date", &["+%Y"], "").1.trim().len(), 4);
        assert_eq!(run_with("date", &["+%Q"], "").0, 1);
        assert!(wildcard(
            &['*', '.', 'r', 's'],
            &"main.rs".chars().collect::<Vec<_>>()
        ));
        assert!(!wildcard(&['?', 'x'], &"abx".chars().collect::<Vec<_>>()));
    }

    #[test]
    fn boxes_and_tables() {
        let b = boxed("notes.txt", "hi\nthere", 80);
        let plain: String = b.replace("\x1b[2m", "").replace("\x1b[0m", "");
        assert_eq!(
            plain,
            "\u{256d}\u{2500} notes.txt \u{2500}\u{256e}\n\
             \u{2502} hi          \u{2502}\n\
             \u{2502} there       \u{2502}\n\
             \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n"
        );
        // Long lines wrap inside the box.
        let b = boxed("t", &"x".repeat(30), 14);
        assert_eq!(b.replace("\x1b[0m", "").matches("\u{2502} ").count(), 3);
        let t = table(["A", "BB"], &[["xyz".into(), "1".into()]], &[1]);
        assert_eq!(t, "A   | BB\nxyz |  1\n");
    }

    #[test]
    fn grep_flags() {
        let text = "Apple pie\nbanana\napple tart\n";
        assert_eq!(
            run_with("grep", &["-i", "^apple"], text),
            (0, "Apple pie\napple tart\n".into(), "".into())
        );
        assert_eq!(
            run_with("grep", &["-vn", "apple"], text).1,
            "1:Apple pie\n2:banana\n"
        );
        assert_eq!(run_with("grep", &["-c", "an"], text).1, "1\n");
        assert_eq!(run_with("grep", &["-F", "."], text).0, 1);
        assert_eq!(run_with("grep", &["("], text).0, 2);
    }

    #[test]
    fn find_and_du() {
        let dir = std::env::temp_dir().join(format!("tacoshell-find-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.join("src/deep/lib.rs"), "").unwrap();
        fs::write(dir.join("README.md"), "").unwrap();
        let d = dir.display().to_string();
        let found = run_with("find", &[&d, "-name", "*.rs"], "").1;
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            found,
            format!("{d}{sep}src{sep}deep{sep}lib.rs\n{d}{sep}src{sep}main.rs\n")
        );
        let folders = run_with("find", &[&d, "-type", "d", "-maxdepth", "1"], "").1;
        assert_eq!(folders, format!("{d}\n{d}{sep}src\n"));
        let du = run_with("du", &[&dir.join("src").display().to_string()], "").1;
        assert!(du.starts_with("SIZE | NAME\n12 B | "), "{du}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_commands() {
        let dir = std::env::temp_dir().join(format!("tacoshell-commands-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let d = |p: &str| dir.join(p).display().to_string();
        assert_eq!(run_with("mkdir", &["-p", &d("a/b")], "").0, 0);
        assert_eq!(run_with("touch", &[&d("a/b/f.txt")], "").0, 0);
        assert_eq!(run_with("tee", &[&d("a/t.txt")], "hi\n").1, "hi\n");
        assert_eq!(fs::read_to_string(dir.join("a/t.txt")).unwrap(), "hi\n");
        // cp needs -r for folders; rm needs -r for folders.
        assert_eq!(run_with("cp", &[&d("a"), &d("c")], "").0, 1);
        assert_eq!(run_with("cp", &["-r", &d("a"), &d("c")], "").0, 0);
        assert!(dir.join("c/b/f.txt").is_file());
        assert_eq!(run_with("mv", &[&d("c/t.txt"), &d("moved.txt")], "").0, 0);
        assert!(dir.join("moved.txt").is_file() && !dir.join("c/t.txt").exists());
        assert_eq!(run_with("rm", &[&d("c")], "").0, 1);
        assert_eq!(run_with("rm", &["-r", &d("c"), &d("moved.txt")], "").0, 0);
        assert!(!dir.join("c").exists() && !dir.join("moved.txt").exists());
        assert_eq!(run_with("rm", &["-f", &d("nothing")], "").0, 0);
        assert_eq!(run_with("rm", &[&d("nothing")], "").0, 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
