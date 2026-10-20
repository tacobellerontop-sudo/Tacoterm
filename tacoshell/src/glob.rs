//! File name patterns: `*`, `?` and `**` (any number of folders), matched
//! without regard to case on Windows. Hidden names (starting with `.`) only
//! match a pattern that starts with `.` too.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tok {
    Char(char),
    /// `*`: any run of characters
    Star,
    /// `?`: any one character
    One,
}

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Whether `name` matches `pat`.
pub fn matches(pat: &[Tok], name: &[char]) -> bool {
    let same = |a: char, b: char| {
        if cfg!(windows) {
            a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };
    // Classic backtracking over the last `*`.
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pat.get(p) {
            Some(Tok::Star) => {
                star = Some((p, n));
                p += 1;
            }
            Some(Tok::One) => {
                p += 1;
                n += 1;
            }
            Some(Tok::Char(c)) if same(*c, name[n]) => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pat[p..].iter().all(|t| *t == Tok::Star)
}

/// The paths matching `pat`, sorted; empty if none do.
pub fn glob(pat: &[Tok]) -> Vec<String> {
    // Split into folder steps, each with the separator after it.
    let mut steps: Vec<(&[Tok], Option<char>)> = Vec::new();
    let mut start = 0;
    for (i, t) in pat.iter().enumerate() {
        if let Tok::Char(c) = t {
            if is_sep(*c) {
                steps.push((&pat[start..i], Some(*c)));
                start = i + 1;
            }
        }
    }
    steps.push((&pat[start..], None));

    let mut found = vec![String::new()];
    let last = steps.len() - 1;
    for (i, (step, sep)) in steps.iter().enumerate() {
        let sep = sep.map(String::from).unwrap_or_default();
        let wild = step.iter().any(|t| !matches!(t, Tok::Char(_)));
        if !wild {
            let text: String = step
                .iter()
                .map(|t| match t {
                    Tok::Char(c) => *c,
                    _ => unreachable!(),
                })
                .collect();
            for f in &mut found {
                f.push_str(&text);
                f.push_str(&sep);
            }
            continue;
        }
        let mut next = Vec::new();
        for prefix in &found {
            let dir = if prefix.is_empty() {
                "."
            } else {
                prefix.as_str()
            };
            if *step == [Tok::Star, Tok::Star] {
                // `**`: this folder and every one below it (and, at the end,
                // every file in them).
                if i < last {
                    next.push(prefix.clone());
                }
                walk(Path::new(dir), prefix, &sep, i == last, &mut next);
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let chars: Vec<char> = name.chars().collect();
                if chars.first() == Some(&'.') && step.first() != Some(&Tok::Char('.')) {
                    continue;
                }
                if !matches(step, &chars) {
                    continue;
                }
                if i < last && !entry.path().is_dir() {
                    continue;
                }
                next.push(format!("{prefix}{name}{sep}"));
            }
        }
        found = next;
        if found.is_empty() {
            return found;
        }
    }
    // Plain steps after the last pattern have to exist.
    found.retain(|f| std::fs::symlink_metadata(f).is_ok());
    found.sort_by_key(|f| f.to_lowercase());
    found.dedup();
    found
}

/// Everything below `dir` (skipping hidden names): folders (with `sep`
/// after them) and, when `files` is set, files too.
fn walk(dir: &Path, prefix: &str, sep: &str, files: bool, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let sep = if sep.is_empty() {
        std::path::MAIN_SEPARATOR_STR
    } else {
        sep
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = format!("{prefix}{name}");
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            if !files {
                out.push(format!("{path}{sep}"));
            } else {
                out.push(path.clone());
            }
            walk(&entry.path(), &format!("{path}{sep}"), sep, files, out);
        } else if files {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> Vec<Tok> {
        s.chars()
            .map(|c| match c {
                '*' => Tok::Star,
                '?' => Tok::One,
                c => Tok::Char(c),
            })
            .collect()
    }

    fn m(p: &str, name: &str) -> bool {
        matches(&pat(p), &name.chars().collect::<Vec<_>>())
    }

    #[test]
    fn matching() {
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "main.rsx"));
        assert!(m("a*b*c", "aXbYbc"));
        assert!(m("?at", "cat"));
        assert!(!m("?at", "at"));
        assert!(m("*", ""));
        assert!(m("**", "x"));
    }

    #[test]
    fn globbing() {
        let dir = std::env::temp_dir().join(format!("tacoshell-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        for f in [
            "a.txt",
            "b.txt",
            "c.md",
            ".hidden.txt",
            "src/x.rs",
            "src/deep/y.rs",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let base = format!("{}/", dir.display());
        let g = |p: &str| -> Vec<String> {
            glob(&pat(&format!("{base}{p}")))
                .into_iter()
                .map(|f| f[base.len()..].replace('\\', "/"))
                .collect()
        };
        assert_eq!(g("*.txt"), ["a.txt", "b.txt"]);
        assert_eq!(g(".*.txt"), [".hidden.txt"]);
        assert_eq!(g("*/*.rs"), ["src/x.rs"]);
        assert_eq!(g("**/*.rs"), ["src/deep/y.rs", "src/x.rs"]);
        assert_eq!(g("s?c"), ["src"]);
        assert_eq!(g("*/deep/y.rs"), ["src/deep/y.rs"]);
        assert!(g("*.nope").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
