//! Fallback backend (no administrator rights): parallel directory walk with one `GetFileInformationByHandleEx`
//! call per 64 KiB of directory entries — sizes and dates come for free, no per-file syscalls.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{Sender, channel};

use super::{Entry, F_DIR, F_REPARSE, NONE, VolumeIndex, VolumeKind, filetime_to_unix, push_name};
use crate::error::{Error, Result};
use crate::progress::Progress;
use crate::winapi::{self, RawEntry};

/// Entries of one directory, ids `base .. base + entries.len()`.
struct Chunk {
    base: u32,
    entries: Vec<Entry>,
    names: Vec<u8>,
    lnames: Vec<u8>,
}

/// One listed child, used both by the full walk and by incremental directory re-listing.
#[derive(Clone, Debug)]
pub struct Listed {
    pub name: String,
    pub is_dir: bool,
    pub is_reparse: bool,
    pub size: u64,
    pub mtime: u32,
}

impl Listed {
    pub fn from_raw(r: &RawEntry) -> Listed {
        Listed {
            name: String::from_utf16_lossy(r.name),
            is_dir: r.is_dir(),
            is_reparse: r.is_reparse(),
            size: if r.is_dir() { 0 } else { r.size },
            mtime: filetime_to_unix(r.mtime),
        }
    }

    pub fn flags(&self) -> u8 {
        (if self.is_dir { F_DIR } else { 0 }) | (if self.is_reparse { F_REPARSE } else { 0 })
    }
}

pub fn list(dir: &Path) -> std::io::Result<Vec<Listed>> {
    let mut v = Vec::new();
    winapi::list_dir(dir, |r| v.push(Listed::from_raw(&r)))?;
    Ok(v)
}

struct Ctx<'a> {
    next_id: AtomicU32,
    tx: Sender<Chunk>,
    progress: &'a Progress,
}

fn walk_dir<'s, 'a: 's>(scope: &rayon::Scope<'s>, ctx: &'s Ctx<'a>, dir: PathBuf, dir_id: u32) {
    if ctx.progress.is_cancelled() {
        return;
    }
    let mut listed: Vec<Listed> = Vec::new();
    if winapi::list_dir(&dir, |r| listed.push(Listed::from_raw(&r))).is_err() {
        ctx.progress.errors.fetch_add(1, Relaxed);
        return;
    }
    if listed.is_empty() {
        return;
    }
    let base = ctx.next_id.fetch_add(listed.len() as u32, Relaxed);
    let (mut names, mut lnames) = (Vec::with_capacity(listed.len() * 24), Vec::with_capacity(listed.len() * 24));
    let mut entries = Vec::with_capacity(listed.len());
    for l in &listed {
        let (off, len) = push_name(&mut names, &mut lnames, &l.name);
        entries.push(Entry {
            parent: dir_id,
            name_off: off,
            size: l.size,
            mtime: l.mtime,
            name_len: len,
            flags: l.flags(),
            _pad: 0,
        });
    }
    ctx.progress.items.fetch_add(listed.len() as u64, Relaxed);
    let _ = ctx.tx.send(Chunk { base, entries, names, lnames });
    for (i, l) in listed.iter().enumerate() {
        if l.is_dir && !l.is_reparse {
            let sub = dir.join(&l.name);
            let id = base + i as u32;
            scope.spawn(move |s| walk_dir(s, ctx, sub, id));
        }
    }
}

/// Walks `root` (a drive root or any folder) in parallel at normal priority.
pub fn scan_root(root: &Path, progress: &Progress) -> Result<VolumeIndex> {
    scan_root_with(root, progress, false)
}

/// `background = true` runs the walk with low CPU and I/O priority, so a re-scan at program start does not get in
/// the way of whatever the user is doing (an HDD walk takes minutes).
pub fn scan_root_with(root: &Path, progress: &Progress, background: bool) -> Result<VolumeIndex> {
    let root_str = crate::winapi::display_path(root);
    let meta = std::fs::metadata(root).map_err(|e| Error::At { path: root.to_path_buf(), source: e })?;
    if !meta.is_dir() {
        return Err(Error::Unsupported(format!("{root_str} не папка")));
    }
    let threads = (std::thread::available_parallelism().map_or(4, |n| n.get()) * 2).clamp(4, 32);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(move |_| {
            if background {
                crate::winapi::lower_current_thread_priority();
            }
        })
        .build()
        .map_err(|e| Error::Unsupported(e.to_string()))?;
    let (tx, rx) = channel();
    let ctx = Ctx { next_id: AtomicU32::new(1), tx, progress };
    pool.scope(|s| walk_dir(s, &ctx, root.to_path_buf(), 0));
    drop(ctx.tx);
    if progress.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let mut chunks: Vec<Chunk> = rx.into_iter().collect();
    chunks.sort_unstable_by_key(|c| c.base);

    let mut vol = VolumeIndex::new(VolumeKind::Walk, &root_str);
    let total: usize = 1 + chunks.iter().map(|c| c.entries.len()).sum::<usize>();
    let mut entries = Vec::with_capacity(total);
    entries.push(vol.entries[0]);
    for c in chunks {
        debug_assert_eq!(c.base as usize, entries.len());
        let shift = vol.names.len() as u32;
        vol.names.extend_from_slice(&c.names);
        vol.lnames.extend_from_slice(&c.lnames);
        entries.extend(c.entries.into_iter().map(|e| Entry { name_off: e.name_off + shift, ..e }));
    }
    vol.live = entries.len();
    vol.entries = entries;
    vol.rebuild_children();
    debug_assert!(vol.entries.iter().all(|e| e.parent != NONE || std::ptr::eq(e, &vol.entries[0])));
    Ok(vol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_a_tree_and_builds_paths() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("a/b/c")).unwrap();
        std::fs::create_dir_all(r.join("Тест")).unwrap();
        std::fs::write(r.join("a/one.txt"), b"hello").unwrap();
        std::fs::write(r.join("a/b/two.bin"), vec![0u8; 3000]).unwrap();
        std::fs::write(r.join("a/b/c/Три.MP4"), b"x").unwrap();
        std::fs::write(r.join("Тест/four.txt"), b"").unwrap();
        let v = scan_root(r, &Progress::default()).unwrap();
        assert_eq!(v.len(), 1 + 4 + 4, "root + a,b,c,Тест + four files");
        let find = |n: &str| (0..v.entries.len() as u32).find(|&i| v.name(i) == n).unwrap();
        let two = find("two.bin");
        assert_eq!(v.entries[two as usize].size, 3000);
        assert!(v.path(two).ends_with(r"a\b\two.bin"), "{}", v.path(two));
        assert!(v.path(find("Три.MP4")).ends_with(r"a\b\c\Три.MP4"));
        assert_eq!(v.lname(find("Три.MP4")), "три.mp4");
        assert!(v.entries[find("a") as usize].is_dir());
        assert!(v.entries[two as usize].mtime > 1_600_000_000);
    }

    #[test]
    fn junctions_are_listed_but_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("real")).unwrap();
        std::fs::write(r.join("real/f.txt"), b"1").unwrap();
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(r.join("jn"))
            .arg(r.join("real"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return; // junction creation unavailable: nothing to check
        }
        let v = scan_root(r, &Progress::default()).unwrap();
        let hits = (0..v.entries.len() as u32).filter(|&i| v.name(i) == "f.txt").count();
        assert_eq!(hits, 1, "the file must not be indexed twice through the junction");
        let jn = (0..v.entries.len() as u32).find(|&i| v.name(i) == "jn").unwrap();
        assert!(v.entries[jn as usize].flags & F_REPARSE != 0);
    }
}
