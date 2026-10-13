//! Enumerates the sources of a transfer into directory and file tasks (sizes known up front for progress/ETA).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

use super::{Control, ItemError, Op, TransferProgress};
use crate::error::{Error, IoContext, Result};
use crate::winapi::{self, RawEntry};

#[derive(Clone, Debug)]
pub struct FileTask {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct DirTask {
    pub src: PathBuf,
    pub dst: PathBuf,
}

#[derive(Default)]
pub struct Plan {
    /// Parents come before children.
    pub dirs: Vec<DirTask>,
    pub files: Vec<FileTask>,
    /// Items that cannot be transferred (symlinks, unreadable folders).
    pub skipped: Vec<ItemError>,
    pub bytes: u64,
}

/// `name (копия).ext`, `name (копия 2).ext`, ... — the first name that is free in `dir`.
pub fn unique_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut n = 1;
    loop {
        let cand = if n == 1 { format!("{stem} (копия){ext}") } else { format!("{stem} (копия {n}){ext}") };
        let path = dir.join(cand);
        if std::fs::symlink_metadata(&path).is_err() {
            return path;
        }
        n += 1;
    }
}

pub fn build(sources: &[PathBuf], dest_dir: &Path, op: Op, ctrl: &Control, prog: &TransferProgress) -> Result<Plan> {
    let mut plan = Plan::default();
    for src in sources {
        ctrl.check()?;
        let meta = std::fs::symlink_metadata(winapi::long_path(src)).at(src)?;
        let name = src.file_name().ok_or_else(|| Error::Unsupported(format!("нет имени у {}", src.display())))?;
        let mut dst = dest_dir.join(name);
        if op == Op::Copy && crate::guard::normalize(&dst) == crate::guard::normalize(src) {
            dst = unique_name(dest_dir, name);
        }
        if meta.file_type().is_symlink() || meta.file_attributes_reparse() {
            plan.skipped.push(ItemError {
                path: src.clone(),
                message: "символическая ссылка / точка соединения пропущена".into(),
            });
            continue;
        }
        if meta.is_dir() {
            plan.dirs.push(DirTask { src: src.clone(), dst: dst.clone() });
            walk(src, &dst, &mut plan, ctrl, prog)?;
        } else {
            plan.bytes += meta.len();
            plan.files.push(FileTask { src: src.clone(), dst, size: meta.len() });
        }
    }
    prog.files_total.store(plan.files.len() as u64, Relaxed);
    Ok(plan)
}

trait ReparseExt {
    fn file_attributes_reparse(&self) -> bool;
}

impl ReparseExt for std::fs::Metadata {
    fn file_attributes_reparse(&self) -> bool {
        use std::os::windows::fs::MetadataExt;
        self.file_attributes() & winapi::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

fn walk(src: &Path, dst: &Path, plan: &mut Plan, ctrl: &Control, prog: &TransferProgress) -> Result<()> {
    struct Child {
        name: std::ffi::OsString,
        is_dir: bool,
        reparse: bool,
        size: u64,
    }
    let mut kids: Vec<Child> = Vec::new();
    let r = winapi::list_dir(src, |e: RawEntry| {
        use std::os::windows::ffi::OsStringExt;
        kids.push(Child {
            name: std::ffi::OsString::from_wide(e.name),
            is_dir: e.is_dir(),
            reparse: e.is_reparse(),
            size: e.size,
        });
    });
    if let Err(e) = r {
        plan.skipped.push(ItemError { path: src.to_path_buf(), message: format!("папка не прочитана: {e}") });
        return Ok(());
    }
    ctrl.check()?;
    for k in kids {
        let (s, d) = (src.join(&k.name), dst.join(&k.name));
        if k.reparse {
            plan.skipped.push(ItemError {
                path: s,
                message: "символическая ссылка / точка соединения пропущена".into(),
            });
        } else if k.is_dir {
            plan.dirs.push(DirTask { src: s.clone(), dst: d.clone() });
            walk(&s, &d, plan, ctrl, prog)?;
        } else {
            plan.bytes += k.size;
            plan.files.push(FileTask { src: s, dst: d, size: k.size });
        }
    }
    prog.files_total.store(plan.files.len() as u64, Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"1").unwrap();
        std::fs::write(dir.path().join("a (копия).txt"), b"1").unwrap();
        let n = unique_name(dir.path(), std::ffi::OsStr::new("a.txt"));
        assert_eq!(n.file_name().unwrap().to_string_lossy(), "a (копия 2).txt");
    }

    #[test]
    fn plan_counts_files_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("src");
        std::fs::create_dir_all(s.join("x/y")).unwrap();
        std::fs::write(s.join("a"), vec![0u8; 10]).unwrap();
        std::fs::write(s.join("x/b"), vec![0u8; 20]).unwrap();
        std::fs::write(s.join("x/y/c"), vec![0u8; 30]).unwrap();
        let d = dir.path().join("dst");
        let p = build(&[s], &d, Op::Copy, &Control::default(), &TransferProgress::default()).unwrap();
        assert_eq!(p.files.len(), 3);
        assert_eq!(p.bytes, 60);
        assert_eq!(p.dirs.len(), 3);
        assert!(p.dirs[0].dst.ends_with("src"));
        assert!(p.files.iter().all(|f| f.dst.starts_with(d.join("src"))));
    }
}
