//! Finding programs: PATH plus PATHEXT on Windows, the executable bit
//! elsewhere.

use std::path::{Path, PathBuf};

/// Extensions tried, in order, for a bare command name.
pub fn path_exts() -> Vec<String> {
    if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_default();
        let mut exts: Vec<String> = pathext
            .split(';')
            .filter(|e| e.starts_with('.'))
            .map(|e| e.to_lowercase())
            .collect();
        if exts.is_empty() {
            exts = [".com", ".exe", ".bat", ".cmd"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        // PowerShell scripts run through PowerShell, like they do in pwsh.
        if !exts.iter().any(|e| e == ".ps1") {
            exts.push(".ps1".to_string());
        }
        exts
    } else {
        Vec::new()
    }
}

fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// Try `base` itself (if it already has a known extension) and then `base`
/// plus each extension.
fn with_exts(base: &Path, exts: &[String]) -> Option<PathBuf> {
    if exts.is_empty() {
        return is_runnable(base).then(|| base.to_path_buf());
    }
    let has_ext = base
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| x[1..].eq_ignore_ascii_case(e)));
    if has_ext && is_runnable(base) {
        return Some(base.to_path_buf());
    }
    exts.iter()
        .map(|ext| {
            let mut s = base.as_os_str().to_os_string();
            s.push(ext);
            PathBuf::from(s)
        })
        .find(|p| is_runnable(p))
}

/// Find a program the way cmd does: a name with a slash in it is a path
/// (relative to `cwd`); otherwise the current folder (Windows only) and then
/// each folder on `path`.
pub fn find_in(name: &str, cwd: &Path, path: &std::ffi::OsStr, exts: &[String]) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains('\\') || Path::new(name).is_absolute() {
        return with_exts(&cwd.join(name), exts);
    }
    let here = cfg!(windows).then(|| cwd.to_path_buf());
    here.into_iter()
        .chain(std::env::split_paths(path))
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| with_exts(&dir.join(name), exts))
}

pub fn find(name: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    find_in(name, &cwd, &path, &path_exts())
}

/// cmd.exe's built-in commands, which have no program of their own; when one
/// isn't found on PATH (some, like `where`, also exist as programs), it runs
/// through `cmd /c`.
pub const CMD_BUILTINS: &[&str] = &[
    "assoc", "copy", "date", "del", "dir", "erase", "ftype", "md", "mkdir", "mklink", "move", "rd",
    "ren", "rename", "rmdir", "start", "time", "title", "type", "ver", "vol",
];

pub fn is_cmd_builtin(name: &str) -> bool {
    cfg!(windows) && CMD_BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tacoshell-resolve-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make(path: &Path) {
        fs::write(path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn searches_path_in_order() {
        let dir = scratch("order");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let exts: Vec<String> = [".exe", ".cmd"].iter().map(|s| s.to_string()).collect();
        make(&b.join("tool.exe"));
        make(&a.join("tool.cmd"));
        let path = std::env::join_paths([&a, &b]).unwrap();
        let cwd = dir.join("elsewhere");
        assert_eq!(
            find_in("tool", &cwd, &path, &exts),
            Some(a.join("tool.cmd"))
        );
        assert_eq!(
            find_in("tool.exe", &cwd, &path, &exts).map(|p| p.is_file()),
            Some(true)
        );
        assert_eq!(find_in("missing", &cwd, &path, &exts), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        let dir = scratch("relative");
        fs::create_dir_all(dir.join("bin")).unwrap();
        let exts = path_exts();
        let file = if cfg!(windows) { "run.exe" } else { "run" };
        make(&dir.join("bin").join(file));
        let empty = std::ffi::OsString::new();
        let found = find_in("bin/run", &dir, &empty, &exts).unwrap();
        assert!(found.is_file());
        // Folders aren't programs.
        assert_eq!(find_in("bin", &dir, &empty, &exts), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn needs_the_executable_bit() {
        let dir = scratch("exec");
        fs::write(dir.join("plain"), "").unwrap();
        let path = dir.clone().into_os_string();
        assert_eq!(find_in("plain", &dir, &path, &[]), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
