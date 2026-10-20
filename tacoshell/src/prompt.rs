//! The prompt: folder, git branch, how long the last command took, and an
//! arrow that turns red after a failure. `TACOSHELL_PROMPT` replaces it with
//! a template, and a `prompt` function replaces it with whatever it prints.

use std::path::Path;
use std::time::Duration;

pub struct Info<'a> {
    pub cwd: &'a Path,
    pub status: i32,
    pub took: Option<Duration>,
}

/// The current git branch (or short commit when detached) for a folder
/// inside a repository, read straight from `.git` so it's instant.
pub fn git_branch(dir: &Path) -> Option<String> {
    let mut here = Some(dir);
    while let Some(d) = here {
        let git = d.join(".git");
        let git_dir = if git.is_dir() {
            git
        } else if git.is_file() {
            // A worktree or submodule: `gitdir: <path>`.
            let text = std::fs::read_to_string(&git).ok()?;
            let target = text.strip_prefix("gitdir:")?.trim();
            d.join(target)
        } else {
            here = d.parent();
            continue;
        };
        let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
        let head = head.trim();
        return Some(match head.strip_prefix("ref: ") {
            Some(r) => r.strip_prefix("refs/heads/").unwrap_or(r).to_string(),
            None => head.chars().take(7).collect(),
        });
    }
    None
}

/// "850ms", "3.2s", "2m 5s", "1h 2m".
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else if d.as_millis() >= 1000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Commands quicker than this don't show their time.
const SHOW_AFTER: Duration = Duration::from_secs(2);

pub const DEFAULT: &str = "{bold}{blue}{cwd}{reset}{git}{took} {arrow} ";

/// Fill in a template: `{cwd}` `{dir}` `{git}` `{branch}` `{took}`
/// `{status}` `{user}` `{host}` `{time}` `{arrow}` `{nl}`, and colors
/// `{red}` `{green}` `{yellow}` `{blue}` `{magenta}` `{cyan}` `{gray}`
/// `{bold}` `{dim}` `{reset}`. Unknown `{…}` stays as is.
pub fn render(template: &str, info: &Info) -> String {
    let branch = git_branch(info.cwd);
    let took = info.took.filter(|d| *d >= SHOW_AFTER);
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let key = &after[..close];
        let value: Option<String> = match key {
            "cwd" => Some(crate::shell::tilde(info.cwd)),
            "dir" => Some(info.cwd.file_name().map_or_else(
                || info.cwd.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )),
            "git" => Some(
                branch
                    .as_ref()
                    .map(|b| format!(" \x1b[35m({b})\x1b[0m"))
                    .unwrap_or_default(),
            ),
            "branch" => Some(branch.clone().unwrap_or_default()),
            "took" => Some(
                took.map(|d| format!(" \x1b[33m{}\x1b[0m", duration(d)))
                    .unwrap_or_default(),
            ),
            "status" => Some(info.status.to_string()),
            "user" => std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .ok(),
            "host" => std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .ok()
                .or_else(|| Some(String::new())),
            "time" => Some(chrono::Local::now().format("%H:%M").to_string()),
            "arrow" => {
                let color = if info.status == 0 { 32 } else { 31 };
                Some(format!("\x1b[{color}m\u{276F}\x1b[0m"))
            }
            "nl" => Some("\r\n".into()),
            "red" => Some("\x1b[31m".into()),
            "green" => Some("\x1b[32m".into()),
            "yellow" => Some("\x1b[33m".into()),
            "blue" => Some("\x1b[34m".into()),
            "magenta" => Some("\x1b[35m".into()),
            "cyan" => Some("\x1b[36m".into()),
            "gray" => Some("\x1b[90m".into()),
            "bold" => Some("\x1b[1m".into()),
            "dim" => Some("\x1b[2m".into()),
            "reset" => Some("\x1b[0m".into()),
            _ => None,
        };
        match value {
            Some(v) => out.push_str(&v),
            None => out.push_str(&rest[open..open + close + 2]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let dir = std::env::temp_dir().join(format!("tacoshell-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        let info = Info {
            cwd: &dir.join("src"),
            status: 1,
            took: Some(Duration::from_millis(3200)),
        };
        assert_eq!(git_branch(&dir.join("src")).as_deref(), Some("feature/x"));
        assert_eq!(
            render("{dir} [{branch}] {status} {nope} {", &info),
            "src [feature/x] 1 {nope} {"
        );
        let full = render(DEFAULT, &info);
        assert!(full.contains("(feature/x)") && full.contains("3.2s") && full.contains("\x1b[31m"));
        std::fs::write(dir.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(&dir).as_deref(), Some("0123456"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_millis(850)), "850ms");
        assert_eq!(duration(Duration::from_millis(3250)), "3.2s");
        assert_eq!(duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(duration(Duration::from_secs(3720)), "1h 2m");
    }
}
