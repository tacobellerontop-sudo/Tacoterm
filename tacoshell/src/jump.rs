//! `z`: jump to a folder you've been to by part of its name, best match by
//! how often and how lately you went there ("frecency", as in z and
//! zoxide). The list lives in `jumps` in the config folder.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    path: PathBuf,
    rank: f64,
    /// Seconds since 1970 of the last visit
    time: u64,
}

#[derive(Default)]
pub struct Jumps {
    file: Option<PathBuf>,
    entries: Vec<Entry>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lower(p: &Path) -> String {
    p.display().to_string().to_lowercase()
}

impl Jumps {
    /// Load the list from `file` and save visits to it.
    pub fn load(file: PathBuf) -> Jumps {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let entries = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let rank = parts.next()?.parse().ok()?;
                let time = parts.next()?.parse().ok()?;
                let path = PathBuf::from(parts.next()?);
                Some(Entry { path, rank, time })
            })
            .collect();
        Jumps {
            file: Some(file),
            entries,
        }
    }

    pub fn visit(&mut self, path: &Path) {
        if self.file.is_none() {
            return;
        }
        let key = lower(path);
        match self.entries.iter_mut().find(|e| lower(&e.path) == key) {
            Some(e) => {
                e.rank += 1.0;
                e.time = now();
            }
            None => self.entries.push(Entry {
                path: path.to_path_buf(),
                rank: 1.0,
                time: now(),
            }),
        }
        // Old, rarely used folders fade away.
        if self.entries.iter().map(|e| e.rank).sum::<f64>() > 5000.0 {
            for e in &mut self.entries {
                e.rank *= 0.9;
            }
            self.entries.retain(|e| e.rank >= 1.0);
        }
        self.save();
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text: String = self
            .entries
            .iter()
            .map(|e| format!("{}\t{}\t{}\n", e.rank, e.time, e.path.display()))
            .collect();
        let _ = std::fs::write(file, text);
    }

    fn score(e: &Entry, now: u64) -> f64 {
        let age = now.saturating_sub(e.time);
        let factor = match age {
            0..3600 => 4.0,
            3600..86400 => 2.0,
            86400..604800 => 0.5,
            _ => 0.25,
        };
        e.rank * factor
    }

    /// Folders matching every word (in order, ignoring case), best first.
    /// The last word has to be in the folder's own name.
    pub fn matches(&self, words: &[String]) -> Vec<PathBuf> {
        let now = now();
        let words: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        let mut found: Vec<(f64, &Entry)> = self
            .entries
            .iter()
            .filter(|e| {
                let text = lower(&e.path);
                let mut at = 0;
                for w in &words {
                    match text[at..].find(w.as_str()) {
                        Some(i) => at += i + w.len(),
                        None => return false,
                    }
                }
                let name = e
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                words.last().is_none_or(|w| name.contains(w.as_str()))
            })
            .map(|e| (Self::score(e, now), e))
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        found
            .into_iter()
            .map(|(_, e)| e.path.clone())
            .filter(|p| p.is_dir())
            .collect()
    }

    /// The list, best first, with scores.
    pub fn list(&self) -> Vec<(f64, PathBuf)> {
        let now = now();
        let mut all: Vec<(f64, PathBuf)> = self
            .entries
            .iter()
            .map(|e| (Self::score(e, now), e.path.clone()))
            .collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_match_by_frecency() {
        let dir = std::env::temp_dir().join(format!("tacoshell-z-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["projects/tacoterm", "projects/tacoshell", "old/tacoshell"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let mut z = Jumps::load(dir.join("jumps"));
        z.visit(&dir.join("projects/tacoterm"));
        z.visit(&dir.join("old/tacoshell"));
        for _ in 0..3 {
            z.visit(&dir.join("projects/tacoshell"));
        }
        let z = Jumps::load(dir.join("jumps"));
        let w = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(z.matches(&w(&["shell"]))[0], dir.join("projects/tacoshell"));
        assert_eq!(
            z.matches(&w(&["old", "taco"])),
            vec![dir.join("old/tacoshell")]
        );
        assert_eq!(
            z.matches(&w(&["TERM"])),
            vec![dir.join("projects/tacoterm")]
        );
        // "projects" is only in the parent's name, not the folder's own.
        assert!(z.matches(&w(&["projects"])).is_empty());
        assert_eq!(z.list().len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
