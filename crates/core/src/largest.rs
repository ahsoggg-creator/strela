//! "What takes the space": the biggest files and folders below a path. Uses the in-memory index when it covers the
//! path (milliseconds), otherwise a parallel directory walk.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;

use crate::error::Result;
use crate::guard::normalize;
use crate::index::{Index, NONE, VolumeIndex};
use crate::progress::Progress;
use crate::winapi::{self, RawEntry};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Big {
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Largest {
    /// Largest files, biggest first.
    pub files: Vec<Big>,
    /// Folders by total size of everything below them, biggest first.
    pub folders: Vec<Big>,
    pub total_bytes: u64,
    pub total_files: u64,
}

/// Bounded "top K" collector with a cheap threshold test, so most items never touch the lock.
struct TopK {
    k: usize,
    heap: Mutex<BinaryHeap<Reverse<(u64, PathBuf)>>>,
    floor: AtomicU64,
}

impl TopK {
    fn new(k: usize) -> TopK {
        TopK { k, heap: Mutex::new(BinaryHeap::new()), floor: AtomicU64::new(0) }
    }
    fn offer(&self, size: u64, path: impl FnOnce() -> PathBuf) {
        if size <= self.floor.load(Relaxed) && self.k > 0 {
            return;
        }
        let mut h = self.heap.lock().unwrap();
        h.push(Reverse((size, path())));
        if h.len() > self.k {
            h.pop();
        }
        if h.len() == self.k
            && let Some(Reverse((min, _))) = h.peek()
        {
            self.floor.store(*min, Relaxed);
        }
    }
    fn into_sorted(self, dirs: bool) -> Vec<Big> {
        let mut v: Vec<Big> = self
            .heap
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|Reverse((size, path))| Big { path, size, is_dir: dirs })
            .collect();
        v.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        v
    }
}

/// Fast path: everything comes from the index.
pub fn from_index(index: &Index, root: &Path, top: usize) -> Option<Largest> {
    let want = normalize(root);
    for v in &index.volumes {
        let vroot = normalize(Path::new(&v.root));
        if want == vroot || (want.starts_with(&vroot) && want.as_bytes().get(vroot.len()).is_some_and(|&c| c == b'\\' || vroot.ends_with('\\'))) {
            return Some(volume_largest(v, &want, top));
        }
    }
    None
}

fn volume_largest(v: &VolumeIndex, want: &str, top: usize) -> Largest {
    let n = v.entries.len();
    // Directory totals: add each file's size to every ancestor (atomics let all cores do it).
    let totals: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
    // Which entries are inside the requested folder: its id plus everything below, via ancestor walk.
    let root_id = find_dir(v, want);
    let inside = |mut id: u32| -> bool {
        let Some(r) = root_id else { return false };
        let mut guard = 0;
        while id != NONE && guard < 4096 {
            if id == r {
                return true;
            }
            id = v.entries[id as usize].parent;
            guard += 1;
        }
        false
    };
    let files = TopK::new(top);
    let folders = TopK::new(top);
    let total_files = AtomicU64::new(0);
    let total_bytes = AtomicU64::new(0);
    (0..n as u32).into_par_iter().for_each(|id| {
        let e = &v.entries[id as usize];
        if e.is_deleted() || e.is_dir() || id == 0 || !inside(id) {
            return;
        }
        total_files.fetch_add(1, Relaxed);
        total_bytes.fetch_add(e.size, Relaxed);
        files.offer(e.size, || PathBuf::from(v.path(id)));
        let mut p = e.parent;
        let mut guard = 0;
        while p != NONE && guard < 4096 {
            totals[p as usize].fetch_add(e.size, Relaxed);
            if Some(p) == root_id {
                break;
            }
            p = v.entries[p as usize].parent;
            guard += 1;
        }
    });
    (0..n as u32).into_par_iter().for_each(|id| {
        let e = &v.entries[id as usize];
        if e.is_dir() && !e.is_deleted() && Some(id) != root_id {
            let t = totals[id as usize].load(Relaxed);
            if t > 0 && inside(id) {
                folders.offer(t, || PathBuf::from(v.path(id)));
            }
        }
    });
    Largest {
        files: files.into_sorted(false),
        folders: folders.into_sorted(true),
        total_bytes: total_bytes.load(Relaxed),
        total_files: total_files.load(Relaxed),
    }
}

fn find_dir(v: &VolumeIndex, want: &str) -> Option<u32> {
    let vroot = normalize(Path::new(&v.root));
    if want == vroot {
        return Some(0);
    }
    let rest = want.strip_prefix(&vroot)?.trim_start_matches('\\');
    // resolve component by component through the entries (no children map on MFT volumes): candidates by name
    let comps: Vec<&str> = rest.split('\\').filter(|c| !c.is_empty()).collect();
    let last = *comps.last()?;
    (0..v.entries.len() as u32)
        .into_par_iter()
        .filter(|&id| v.entries[id as usize].is_dir() && !v.entries[id as usize].is_deleted() && v.lname(id) == last)
        .find_any(|&id| normalize(Path::new(&v.path(id))) == want)
}

/// Slow path: parallel walk of the folder.
pub fn from_walk(root: &Path, top: usize, progress: &Progress) -> Result<Largest> {
    let files = TopK::new(top);
    let folders = TopK::new(top);
    let total_files = AtomicU64::new(0);
    let total_bytes = AtomicU64::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads((std::thread::available_parallelism().map_or(4, |n| n.get()) * 2).clamp(4, 32))
        .stack_size(16 << 20)
        .build()
        .map_err(|e| crate::error::Error::Unsupported(e.to_string()))?;

    fn walk(dir: &Path, files: &TopK, folders: &TopK, tf: &AtomicU64, tb: &AtomicU64, progress: &Progress) -> u64 {
        if progress.is_cancelled() {
            return 0;
        }
        let mut kids: Vec<(std::ffi::OsString, bool, u64)> = Vec::new();
        let _ = winapi::list_dir(dir, |e: RawEntry| {
            use std::os::windows::ffi::OsStringExt;
            if !e.is_reparse() {
                kids.push((std::ffi::OsString::from_wide(e.name), e.is_dir(), e.size));
            }
        });
        let total: u64 = kids
            .par_iter()
            .map(|(name, is_dir, size)| {
                if *is_dir {
                    walk(&dir.join(name), files, folders, tf, tb, progress)
                } else {
                    tf.fetch_add(1, Relaxed);
                    tb.fetch_add(*size, Relaxed);
                    progress.items.fetch_add(1, Relaxed);
                    files.offer(*size, || dir.join(name));
                    *size
                }
            })
            .sum();
        folders.offer(total, || dir.to_path_buf());
        total
    }
    pool.install(|| {
        walk(root, &files, &folders, &total_files, &total_bytes, progress);
    });
    let mut folders = folders.into_sorted(true);
    folders.retain(|f| f.path != root && f.size > 0);
    Ok(Largest {
        files: files.into_sorted(false),
        folders,
        total_bytes: total_bytes.load(Relaxed),
        total_files: total_files.load(Relaxed),
    })
}

/// Index when it covers the path, walk otherwise.
pub fn find(index: Option<&Index>, root: &Path, top: usize, progress: &Progress) -> Result<Largest> {
    if let Some(r) = index.and_then(|i| from_index(i, root, top)) {
        return Ok(r);
    }
    from_walk(root, top, progress)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::index::walk::scan_root;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("media/video")).unwrap();
        fs::create_dir_all(r.join("docs")).unwrap();
        fs::write(r.join("media/video/big.mkv"), vec![0u8; 900_000]).unwrap();
        fs::write(r.join("media/video/small.mkv"), vec![0u8; 100]).unwrap();
        fs::write(r.join("media/song.mp3"), vec![0u8; 50_000]).unwrap();
        fs::write(r.join("docs/a.txt"), vec![0u8; 10]).unwrap();
        fs::write(r.join("docs/b.txt"), vec![0u8; 20]).unwrap();
        tmp
    }

    fn check(l: &Largest, root: &Path) {
        let names: Vec<_> = l.files.iter().map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names[..3], ["big.mkv", "song.mp3", "small.mkv"], "{names:?}");
        assert_eq!(l.files[0].size, 900_000);
        assert_eq!(l.total_files, 5);
        assert_eq!(l.total_bytes, 900_000 + 100 + 50_000 + 30);
        let f: Vec<_> = l.folders.iter().map(|b| (b.path.strip_prefix(root).unwrap().to_string_lossy().into_owned(), b.size)).collect();
        assert_eq!(f[0], ("media".into(), 950_100), "{f:?}");
        assert_eq!(f[1], ("media\\video".into(), 900_100), "{f:?}");
        assert!(f.contains(&("docs".into(), 30)));
    }

    #[test]
    fn walk_and_index_agree() {
        let tmp = fixture();
        let l = from_walk(tmp.path(), 3, &Progress::default()).unwrap();
        assert_eq!(l.files.len(), 3);
        // top-3 files only, but totals cover everything
        assert_eq!(l.total_files, 5);
        let l = from_walk(tmp.path(), 10, &Progress::default()).unwrap();
        check(&l, tmp.path());

        let mut idx = Index::default();
        idx.set_volume(scan_root(tmp.path(), &Progress::default()).unwrap());
        let l = from_index(&idx, tmp.path(), 10).expect("index covers the folder");
        check(&l, tmp.path());
        // a sub folder is answered from the same index
        let sub = from_index(&idx, &tmp.path().join("media"), 10).unwrap();
        assert_eq!(sub.total_files, 3);
        assert_eq!(sub.folders[0].size, 900_100);
        // outside the index: None, walk is used by `find`
        assert!(from_index(&idx, Path::new("Z:\\nowhere"), 10).is_none());
    }
}
