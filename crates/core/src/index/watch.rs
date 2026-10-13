//! Keeps a walked [`VolumeIndex`] fresh with `ReadDirectoryChangesW`.
//!
//! Change events only mark directories dirty. After a short debounce each dirty directory is listed again and
//! diffed against the index, which also handles renames, moves, bursts and lost events uniformly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use parking_lot::Mutex;

use super::walk::{Listed, list};
use super::{F_DELETED, SharedIndex, VolumeIndex, fold};
use crate::winapi::DirWatcher;

const DEBOUNCE: Duration = Duration::from_millis(600);

/// A subtree discovered while resyncing: `parent` indexes into the same vector (`None` = the resynced directory).
struct SubItem {
    parent: Option<usize>,
    item: Listed,
}

fn collect_subtree(path: &Path) -> Vec<SubItem> {
    let mut out: Vec<SubItem> = Vec::new();
    let mut queue: Vec<(PathBuf, Option<usize>)> = vec![(path.to_path_buf(), None)];
    while let Some((dir, parent)) = queue.pop() {
        let Ok(items) = list(&dir) else { continue };
        for it in items {
            let idx = out.len();
            let descend = it.is_dir && !it.is_reparse;
            let sub = dir.join(&it.name);
            out.push(SubItem { parent, item: it });
            if descend {
                queue.push((sub, Some(idx)));
            }
        }
    }
    out
}

/// Removes `id` and everything below it.
fn remove_subtree(v: &mut VolumeIndex, id: u32) {
    let mut stack = vec![id];
    while let Some(cur) = stack.pop() {
        if let Some(kids) = v.children.get(&cur) {
            stack.extend(kids.iter().copied());
        }
        v.remove(cur);
    }
}

/// Resyncs one directory (`dir` is an absolute path below `root`). Returns true if the index changed.
pub fn resync_dir(index: &SharedIndex, root: &str, dir: &Path) -> bool {
    // Walk up until a directory known to the index is found (a new folder is picked up from its parent).
    let mut dir = dir.to_path_buf();
    let (dir_id, snapshot) = loop {
        let found = {
            let g = index.read();
            g.volume_for_root(root).and_then(|i| {
                let v = &g.volumes[i];
                v.find_dir(&dir.to_string_lossy()).map(|id| (id, snapshot_children(v, id)))
            })
        };
        match found {
            Some(f) => break f,
            None => match dir.parent() {
                Some(p) if p.as_os_str().len() >= root.trim_end_matches('\\').len() => dir = p.to_path_buf(),
                _ => return false,
            },
        }
    };

    let listing = match list(&dir) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return false,
    };
    // New subfolders are walked outside the lock.
    let known: HashSet<&str> = snapshot.iter().map(|c| c.lname.as_str()).collect();
    let mut new_subtrees: HashMap<String, Vec<SubItem>> = HashMap::new();
    for l in &listing {
        if l.is_dir && !l.is_reparse && !known.contains(fold(&l.name).as_str()) {
            new_subtrees.insert(fold(&l.name), collect_subtree(&dir.join(&l.name)));
        }
    }

    let mut g = index.write();
    let Some(vi) = g.volume_for_root(root) else { return false };
    let v = &mut g.volumes[vi];
    if v.entries.get(dir_id as usize).is_none_or(|e| e.flags & F_DELETED != 0) {
        return false;
    }
    let mut changed = false;
    let mut by_name: HashMap<String, &Listed> = HashMap::with_capacity(listing.len());
    for l in &listing {
        by_name.insert(fold(&l.name), l);
    }
    let mut seen: HashSet<String> = HashSet::new();
    for c in &snapshot {
        match by_name.get(&c.lname) {
            None => {
                remove_subtree(v, c.id);
                changed = true;
            }
            Some(l) => {
                seen.insert(c.lname.clone());
                if l.is_dir != c.is_dir {
                    remove_subtree(v, c.id);
                    add_listed(v, dir_id, l, new_subtrees.remove(&c.lname));
                    changed = true;
                    continue;
                }
                if v.name(c.id) != l.name {
                    v.rename(c.id, dir_id, &l.name);
                    changed = true;
                }
                if !l.is_dir && (v.entries[c.id as usize].size != l.size || v.entries[c.id as usize].mtime != l.mtime) {
                    v.set_stat(c.id, l.size, l.mtime);
                    changed = true;
                }
            }
        }
    }
    for l in &listing {
        let key = fold(&l.name);
        if seen.contains(&key) || known.contains(key.as_str()) {
            continue;
        }
        add_listed(v, dir_id, l, new_subtrees.remove(&key));
        changed = true;
    }
    v.compact_if_needed();
    changed
}

struct Child {
    id: u32,
    lname: String,
    is_dir: bool,
}

fn snapshot_children(v: &VolumeIndex, dir: u32) -> Vec<Child> {
    v.children
        .get(&dir)
        .map(|kids| {
            kids.iter()
                .map(|&id| Child { id, lname: v.lname(id).to_string(), is_dir: v.entries[id as usize].is_dir() })
                .collect()
        })
        .unwrap_or_default()
}

fn add_listed(v: &mut VolumeIndex, parent: u32, l: &Listed, subtree: Option<Vec<SubItem>>) {
    let id = v.add(None, parent, &l.name, l.flags(), l.size, l.mtime);
    if let Some(items) = subtree {
        let mut ids: Vec<u32> = Vec::with_capacity(items.len());
        for it in items {
            let p = it.parent.map_or(id, |i| ids[i]);
            ids.push(v.add(None, p, &it.item.name, it.item.flags(), it.item.size, it.item.mtime));
        }
    }
}

/// Parses FILE_NOTIFY_INFORMATION records into paths relative to the watched root.
fn parse_notifications(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 12 <= buf.len() {
        let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
        let nbytes = u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap()) as usize;
        if off + 12 + nbytes > buf.len() {
            break;
        }
        let units = buf[off + 12..off + 12 + nbytes].as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p));
        out.push((action, char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect()));
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

pub struct Watcher {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Watcher {
    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// Starts watching `root` for the walked volume registered under `root` in `index`.
/// `rescan` is called when events were lost (the owner decides how to rescan the whole root).
pub fn start(
    root: PathBuf,
    index: SharedIndex,
    on_change: Arc<dyn Fn() + Send + Sync>,
    rescan: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<Watcher> {
    let mut w = DirWatcher::open(&root)?;
    let stop = Arc::new(AtomicBool::new(false));
    let dirty: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let overflow = Arc::new(AtomicBool::new(false));
    let root_key = crate::winapi::display_path(&root);

    let reader = {
        let (stop, dirty, overflow, root) = (stop.clone(), dirty.clone(), overflow.clone(), root.clone());
        std::thread::Builder::new().name("strela-watch".into()).spawn(move || {
            while !stop.load(Relaxed) {
                match w.next(&stop) {
                    Ok(None) => break,
                    Ok(Some(buf)) if buf.is_empty() => overflow.store(true, Relaxed),
                    Ok(Some(buf)) => {
                        let mut d = dirty.lock();
                        for (_action, rel) in parse_notifications(&buf) {
                            // the containing directory is what changed (rename/delete/create/size alike)
                            let full = root.join(&rel);
                            if let Some(p) = full.parent() {
                                d.insert(p.to_path_buf());
                            }
                            // a directory moved/removed: its own listing is gone, parent covers it
                        }
                    }
                    Err(_) => {
                        overflow.store(true, Relaxed);
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        })?
    };
    let applier = {
        let stop = stop.clone();
        std::thread::Builder::new().name("strela-resync".into()).spawn(move || {
            while !stop.load(Relaxed) {
                std::thread::sleep(DEBOUNCE);
                if overflow.swap(false, Relaxed) {
                    dirty.lock().clear();
                    rescan();
                    continue;
                }
                let batch: Vec<PathBuf> = {
                    let mut d = dirty.lock();
                    d.drain().collect()
                };
                if batch.is_empty() {
                    continue;
                }
                let mut changed = false;
                for dir in batch {
                    if stop.load(Relaxed) {
                        return;
                    }
                    changed |= resync_dir(&index, &root_key, &dir);
                }
                if changed {
                    on_change();
                }
            }
        })?
    };
    Ok(Watcher { stop, threads: vec![reader, applier] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::walk::scan_root;
    use crate::progress::Progress;

    fn names(index: &SharedIndex) -> Vec<String> {
        let g = index.read();
        let v = &g.volumes[0];
        let mut n: Vec<String> = (0..v.entries.len() as u32)
            .filter(|&i| !v.entries[i as usize].is_deleted() && i != 0)
            .map(|i| v.path(i).replace(&v.root_display().to_string(), ""))
            .collect();
        n.sort();
        n
    }

    #[test]
    fn resync_applies_creates_deletes_renames_and_new_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("a")).unwrap();
        std::fs::write(r.join("a/old.txt"), b"1").unwrap();
        std::fs::write(r.join("a/keep.txt"), b"1").unwrap();
        let index = crate::index::shared();
        let vol = scan_root(r, &Progress::default()).unwrap();
        let root = vol.root.clone();
        index.write().set_volume(vol);

        std::fs::remove_file(r.join("a/old.txt")).unwrap();
        std::fs::write(r.join("a/keep.txt"), b"longer content").unwrap();
        std::fs::write(r.join("a/new.txt"), b"x").unwrap();
        std::fs::create_dir_all(r.join("a/sub/deep")).unwrap();
        std::fs::write(r.join("a/sub/deep/f.bin"), b"zz").unwrap();
        assert!(resync_dir(&index, &root, &r.join("a")));
        let n = names(&index);
        assert!(n.contains(&r"\a\new.txt".to_string()), "{n:?}");
        assert!(n.contains(&r"\a\sub\deep\f.bin".to_string()), "{n:?}");
        assert!(!n.iter().any(|p| p.ends_with("old.txt")), "{n:?}");
        {
            let g = index.read();
            let v = &g.volumes[0];
            let k = (0..v.entries.len() as u32).find(|&i| v.name(i) == "keep.txt").unwrap();
            assert_eq!(v.entries[k as usize].size, 14);
        }

        // rename a folder: old subtree disappears, new one appears
        std::fs::rename(r.join("a/sub"), r.join("a/moved")).unwrap();
        assert!(resync_dir(&index, &root, &r.join("a")));
        let n = names(&index);
        assert!(n.contains(&r"\a\moved\deep\f.bin".to_string()), "{n:?}");
        assert!(!n.iter().any(|p| p.contains(r"\sub")), "{n:?}");

        // unchanged directory: no change reported
        assert!(!resync_dir(&index, &root, &r.join("a")));

        // a whole folder vanishes
        std::fs::remove_dir_all(r.join("a/moved")).unwrap();
        assert!(resync_dir(&index, &root, &r.join("a")));
        assert!(!names(&index).iter().any(|p| p.contains("moved")));
    }

    #[test]
    fn live_watcher_picks_up_changes() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path().to_path_buf();
        std::fs::write(r.join("before.txt"), b"1").unwrap();
        let index = crate::index::shared();
        let vol = scan_root(&r, &Progress::default()).unwrap();
        index.write().set_volume(vol);
        let changed = Arc::new(AtomicBool::new(false));
        let c2 = changed.clone();
        let w = start(r.clone(), index.clone(), Arc::new(move || c2.store(true, Relaxed)), Arc::new(|| {})).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(r.join("Новый файл.txt"), b"hello").unwrap();
        let t0 = std::time::Instant::now();
        while !names(&index).iter().any(|p| p.contains("Новый файл")) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(names(&index).iter().any(|p| p.contains("Новый файл")), "{:?}", names(&index));
        assert!(changed.load(Relaxed));
        w.stop();
    }
}
