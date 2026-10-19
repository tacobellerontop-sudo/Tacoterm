//! Tab completion: command names at the start of a command, file and folder
//! names everywhere else.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// What replaces the word being completed.
    pub replace: String,
    /// What's shown when several match.
    pub display: String,
    pub is_dir: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Completion {
    /// Char index where the word being completed starts.
    pub start: usize,
    pub candidates: Vec<Candidate>,
}

/// Where the word under the cursor starts, its text with quotes removed, and
/// whether it's in command position.
fn current_word(chars: &[char]) -> (usize, String, bool) {
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut command_pos = true;
    let mut redirect = false;
    for (i, &c) in chars.iter().enumerate() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>') {
            if i > start {
                // A word just ended: a redirection's target, or a command
                // (after which come its arguments).
                if redirect {
                    redirect = false;
                } else {
                    command_pos = false;
                }
            }
            if matches!(c, '|' | '&' | ';') {
                command_pos = true;
                redirect = false;
            } else if matches!(c, '<' | '>') {
                redirect = true;
            }
            start = i + 1;
        }
    }
    let word: String = chars[start..]
        .iter()
        .filter(|&&c| c != '"' && c != '\'')
        .collect();
    (start, word, command_pos && !redirect)
}

fn starts_with(name: &str, prefix: &str) -> bool {
    if cfg!(windows) {
        name.to_lowercase().starts_with(&prefix.to_lowercase())
    } else {
        name.starts_with(prefix)
    }
}

fn quote_if_needed(s: &str, is_dir: bool) -> String {
    if s.contains(' ') || s.contains('&') {
        // Leave a folder's quote open so completing inside it keeps going.
        if is_dir {
            format!("\"{s}")
        } else {
            format!("\"{s}\"")
        }
    } else {
        s.to_string()
    }
}

pub fn complete(
    chars: &[char],
    cwd: &Path,
    home: Option<&Path>,
    commands: &dyn Fn() -> Vec<String>,
) -> Completion {
    let (start, word, command_pos) = current_word(chars);
    let mut candidates = Vec::new();
    if command_pos && !word.contains('/') && !word.contains('\\') && !word.is_empty() {
        let mut names: Vec<String> = commands()
            .into_iter()
            .filter(|n| starts_with(n, &word))
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        candidates = names
            .into_iter()
            .map(|n| Candidate {
                replace: n.clone(),
                display: n,
                is_dir: false,
            })
            .collect();
    }
    if candidates.is_empty() {
        candidates = complete_path(&word, cwd, home);
    }
    Completion { start, candidates }
}

fn complete_path(word: &str, cwd: &Path, home: Option<&Path>) -> Vec<Candidate> {
    let split = word.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let (dir_part, prefix) = word.split_at(split);
    let dir: PathBuf = if let (Some(rest), Some(home)) = (dir_part.strip_prefix('~'), home) {
        home.join(rest.trim_start_matches(['/', '\\']))
    } else if dir_part.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(dir_part)
    };
    let sep = if dir_part.contains('/') || !cfg!(windows) {
        '/'
    } else {
        '\\'
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<Candidate> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !starts_with(&name, prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let is_dir = e.path().is_dir();
            let mut full = format!("{dir_part}{name}");
            let mut display = name;
            if is_dir {
                full.push(sep);
                display.push(sep);
            }
            Some(Candidate {
                replace: quote_if_needed(&full, is_dir),
                display,
                is_dir,
            })
        })
        .collect();
    out.sort_by_key(|c| c.display.to_lowercase());
    out
}

/// The longest prefix all candidates share (ignoring case on Windows),
/// spelled as in the first one.
pub fn common_prefix(candidates: &[Candidate]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut len = first.replace.chars().count();
    for c in &candidates[1..] {
        len = first
            .replace
            .chars()
            .zip(c.replace.chars())
            .take(len)
            .take_while(|(a, b)| {
                if cfg!(windows) {
                    a.eq_ignore_ascii_case(b)
                } else {
                    a == b
                }
            })
            .count();
    }
    first.replace.chars().take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn common_prefixes() {
        let cand = |s: &str| Candidate {
            replace: s.into(),
            display: s.into(),
            is_dir: false,
        };
        assert_eq!(common_prefix(&[cand("sorted"), cand("sort")]), "sort");
        assert_eq!(common_prefix(&[cand("abc")]), "abc");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn finds_the_current_word() {
        assert_eq!(current_word(&chars("gi")), (0, "gi".into(), true));
        assert_eq!(current_word(&chars("git sta")), (4, "sta".into(), false));
        assert_eq!(current_word(&chars("ls | so")), (5, "so".into(), true));
        assert_eq!(
            current_word(&chars("cd \"My Doc")),
            (3, "My Doc".into(), false)
        );
        assert_eq!(
            current_word(&chars("echo hi > ou")),
            (10, "ou".into(), false)
        );
        assert_eq!(current_word(&chars("a && ")), (5, "".into(), true));
    }

    #[test]
    fn completes_commands_then_paths() {
        let dir = std::env::temp_dir().join(format!("tacoshell-complete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src dir")).unwrap();
        fs::write(dir.join("sample.txt"), "").unwrap();
        fs::write(dir.join(".hidden"), "").unwrap();
        let cmds = || vec!["sort".to_string(), "set".to_string(), "Sort".to_string()];

        let c = complete(&chars("so"), &dir, None, &cmds);
        assert_eq!(c.candidates.len(), 1);
        assert_eq!(c.candidates[0].replace, "sort");

        let c = complete(&chars("cat s"), &dir, None, &cmds);
        let names: Vec<&str> = c.candidates.iter().map(|c| c.display.as_str()).collect();
        let sep = if cfg!(windows) { "\\" } else { "/" };
        assert_eq!(
            names,
            vec!["sample.txt".to_string(), format!("src dir{sep}")]
        );
        assert_eq!(c.candidates[1].replace, format!("\"src dir{sep}"));

        // Inside a quoted folder.
        let c = complete(&chars("cd \"src dir/"), &dir, None, &cmds);
        assert_eq!((c.start, c.candidates.len()), (3, 0));

        // Dot files only when asked for.
        let c = complete(&chars("cat ."), &dir, None, &cmds);
        assert_eq!(c.candidates[0].display, ".hidden");
        fs::remove_dir_all(&dir).unwrap();
    }
}
