//! Protection against catastrophic deletions (and moves, which remove the source).
//!
//! Drive roots, `C:\Windows` (the whole tree), `Program Files`, `Program Files (x86)`, `ProgramData`, the user
//! profile and everything that contains one of them (`C:\Users`, ...) can only be deleted after the caller
//! passes the folder's name typed by the user.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    pub reason: String,
    /// What the user has to type to confirm (folder name, or `C:` for a drive root).
    pub required_name: String,
}

pub struct Guard {
    /// Exact folders (normalised) that must not be deleted without typed confirmation.
    exact: Vec<(String, &'static str)>,
    /// Folders whose whole subtree is protected.
    trees: Vec<(String, &'static str)>,
}

/// Lexically normalises a path: strips `\\?\`, unifies separators, removes `.`/`..`, expands short names when the
/// path exists, lowercases, drops a trailing separator.
pub fn normalize(p: &Path) -> String {
    let raw = crate::winapi::display_path(p);
    let mut path = PathBuf::new();
    for c in Path::new(&raw).components() {
        match c {
            Component::ParentDir => {
                path.pop();
            }
            Component::CurDir => {}
            other => path.push(other.as_os_str()),
        }
    }
    let expanded = crate::winapi::long_name(&path).unwrap_or(path);
    let mut s = crate::winapi::display_path(&expanded).replace('/', "\\").to_lowercase();
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

fn is_drive_root(norm: &str) -> bool {
    let b = norm.as_bytes();
    (b.len() == 2 || (b.len() == 3 && b[2] == b'\\')) && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn env_path(var: &str) -> Option<String> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(|v| normalize(Path::new(&v)))
}

impl Guard {
    /// Protected set derived from this machine's environment.
    pub fn system() -> Guard {
        let mut exact = Vec::new();
        let mut trees = Vec::new();
        if let Some(p) = env_path("SystemRoot").or_else(|| env_path("windir")) {
            trees.push((p, "системная папка Windows"));
        }
        for (var, why) in [
            ("ProgramFiles", "Program Files"),
            ("ProgramFiles(x86)", "Program Files (x86)"),
            ("ProgramW6432", "Program Files"),
            ("ProgramData", "ProgramData"),
            ("USERPROFILE", "профиль пользователя"),
            ("ALLUSERSPROFILE", "ProgramData"),
            ("PUBLIC", "общий профиль"),
        ] {
            if let Some(p) = env_path(var) {
                exact.push((p, why));
            }
        }
        Guard::from_parts(exact, trees)
    }

    /// For tests and for callers that need custom roots.
    pub fn from_parts(exact: Vec<(String, &'static str)>, trees: Vec<(String, &'static str)>) -> Guard {
        Guard { exact, trees }
    }

    /// `Some` if deleting `path` must be confirmed by typing a name.
    pub fn check(&self, path: &Path) -> Option<Protected> {
        let norm = normalize(path);
        if is_drive_root(&norm) {
            let drive = norm[..2].to_uppercase();
            return Some(Protected {
                reason: format!("корень диска {drive}\\"),
                required_name: drive,
            });
        }
        let name_of = || {
            Path::new(&crate::winapi::display_path(path))
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| norm.clone())
        };
        let sep = |a: &str, b: &str| a.len() > b.len() && a.starts_with(b) && a.as_bytes()[b.len()] == b'\\';
        for (p, why) in &self.trees {
            if norm == *p || sep(&norm, p) {
                return Some(Protected {
                    reason: format!("{why} ({})", p),
                    required_name: name_of(),
                });
            }
        }
        for (p, why) in &self.exact {
            // The folder itself, or any ancestor of it (deleting `C:\Users` removes the profile too).
            if norm == *p || sep(p, &norm) {
                return Some(Protected {
                    reason: format!("{why} ({})", p),
                    required_name: name_of(),
                });
            }
        }
        for (p, why) in &self.trees {
            if sep(p, &norm) {
                return Some(Protected {
                    reason: format!("содержит {why} ({})", p),
                    required_name: name_of(),
                });
            }
        }
        None
    }

    /// Ok if the path is not protected or `typed` matches the required name (case-insensitive).
    pub fn ensure_deletable(&self, path: &Path, typed: Option<&str>) -> Result<()> {
        match self.check(path) {
            None => Ok(()),
            Some(p) => match typed {
                Some(t) if t.trim().eq_ignore_ascii_case(&p.required_name) => Ok(()),
                _ => Err(Error::Protected(format!(
                    "{} — {}; для подтверждения нужно ввести «{}»",
                    crate::winapi::display_path(path),
                    p.reason,
                    p.required_name
                ))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> Guard {
        Guard::from_parts(
            vec![
                (r"c:\program files".into(), "Program Files"),
                (r"c:\programdata".into(), "ProgramData"),
                (r"c:\users\vasya".into(), "профиль пользователя"),
            ],
            vec![(r"c:\windows".into(), "Windows")],
        )
    }

    #[test]
    fn drive_roots_are_protected() {
        for p in [r"C:\", r"C:", r"d:\", r"\\?\E:\"] {
            let r = g().check(Path::new(p)).unwrap_or_else(|| panic!("{p} must be protected"));
            assert!(r.required_name.ends_with(':'), "{r:?}");
        }
    }

    #[test]
    fn system_folders_and_their_ancestors() {
        let g = g();
        assert!(g.check(Path::new(r"C:\Windows")).is_some());
        assert!(g.check(Path::new(r"c:\WINDOWS\System32")).is_some(), "whole Windows tree");
        assert!(g.check(Path::new(r"C:\Windows\")).is_some(), "trailing separator");
        assert!(g.check(Path::new(r"C:\Program Files")).is_some());
        assert!(g.check(Path::new(r"C:\ProgramData")).is_some());
        assert!(g.check(Path::new(r"C:\Users\Vasya")).is_some());
        assert!(g.check(Path::new(r"C:\Users")).is_some(), "ancestor of the profile");
    }

    #[test]
    fn tricks_do_not_bypass() {
        let g = g();
        assert!(g.check(Path::new(r"C:\Users\Vasya\..\..\Windows")).is_some(), "dot-dot");
        assert!(g.check(Path::new(r"\\?\C:\Windows")).is_some(), "extended prefix");
        assert!(g.check(Path::new("C:/Windows")).is_some(), "forward slash");
        assert!(g.check(Path::new(r"C:\Windows\.")).is_some());
    }

    #[test]
    fn ordinary_paths_are_allowed() {
        let g = g();
        assert!(g.check(Path::new(r"C:\Users\Vasya\Downloads")).is_none());
        assert!(g.check(Path::new(r"C:\Program Files\SomeApp")).is_none());
        assert!(g.check(Path::new(r"D:\Games\Old")).is_none());
        assert!(g.check(Path::new(r"C:\Windows2")).is_none(), "prefix match is not a match");
        assert!(g.check(Path::new(r"C:\Users\VasyaTwo")).is_none());
    }

    #[test]
    fn typed_confirmation_unlocks() {
        let g = g();
        let p = Path::new(r"C:\Windows");
        assert!(matches!(g.ensure_deletable(p, None), Err(Error::Protected(_))));
        assert!(matches!(g.ensure_deletable(p, Some("system32")), Err(Error::Protected(_))));
        assert!(g.ensure_deletable(p, Some("windows")).is_ok());
        assert!(g.ensure_deletable(Path::new(r"C:\"), Some("C:")).is_ok());
        assert!(g.ensure_deletable(Path::new(r"C:\"), Some("D:")).is_err());
        assert!(g.ensure_deletable(Path::new(r"D:\Games"), None).is_ok());
    }

    #[test]
    fn system_guard_covers_this_machine() {
        let g = Guard::system();
        if let Some(root) = std::env::var_os("SystemRoot") {
            assert!(g.check(Path::new(&root)).is_some());
        }
        if let Some(p) = std::env::var_os("USERPROFILE") {
            assert!(g.check(Path::new(&p)).is_some());
        }
        if let Some(p) = std::env::var_os("ProgramFiles") {
            assert!(g.check(Path::new(&p)).is_some());
        }
        // 8.3 form of Program Files resolves to the same folder (only where short names exist on this volume)
        if Path::new(r"C:\PROGRA~1").exists() {
            assert!(g.check(Path::new(r"C:\PROGRA~1")).is_some());
        }
    }
}
