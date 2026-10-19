//! The everyday commands other shells give you (cat, grep, head, rm, cp…),
//! built in so they work the same on every Windows machine. They read
//! files, or their input (a pipe, `< file`, or the console) when given none.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const NAMES: &[&str] = &[
    "basename", "cat", "cp", "dirname", "grep", "head", "img", "mkdir", "mv", "open", "realpath",
    "rm", "sleep", "sort", "tail", "tee", "touch", "uniq", "wc",
];

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

/// Run built-in `name`; `None` if it isn't one of these.
pub fn run(
    name: &str,
    args: &[String],
    input: &mut Input,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> Option<i32> {
    let with_value: &[char] = match name {
        "head" | "tail" => &['n'],
        "img" => &['w'],
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
        let code = run(name, &args, &mut input, &mut out, &mut err).unwrap();
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
