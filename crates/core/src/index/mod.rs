//! In-memory file index.
//!
//! One [`VolumeIndex`] per NTFS volume (read from the MFT, kept fresh with the USN journal) or per walked root
//! (parallel directory walk, kept fresh with `ReadDirectoryChangesW`). Entries are fixed-size records pointing
//! into two byte arenas — the original names and their lowercase twins with identical offsets — so a name search
//! is one SIMD substring scan over a single buffer.

pub mod mft;
pub mod ntfs;
pub mod store;
pub mod usn;
pub mod walk;
pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

pub const NONE: u32 = u32::MAX;

pub const F_DIR: u8 = 1;
pub const F_DELETED: u8 = 2;
/// Symlink / junction: listed, but never descended into.
pub const F_REPARSE: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Entry {
    pub parent: u32,
    pub name_off: u32,
    pub size: u64,
    /// Last write time, Unix seconds.
    pub mtime: u32,
    pub name_len: u16,
    pub flags: u8,
    pub _pad: u8,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & F_DELETED != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    /// Read straight from `$MFT`, kept fresh with the USN journal (administrator / service).
    Mft,
    /// Parallel directory walk, kept fresh with `ReadDirectoryChangesW`.
    Walk,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsnPosition {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// Appends `name` to the arenas (original + lowercase twin of identical length), returns its offset.
pub fn push_name(names: &mut Vec<u8>, lnames: &mut Vec<u8>, name: &str) -> (u32, u16) {
    let off = names.len() as u32;
    names.extend_from_slice(name.as_bytes());
    if name.is_ascii() {
        lnames.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
    } else {
        for ch in name.chars() {
            let mut lower = ch.to_lowercase();
            match (lower.next(), lower.next()) {
                // Only fold when the byte length stays the same; offsets must be shared by both arenas.
                (Some(l), None) if l.len_utf8() == ch.len_utf8() => {
                    let mut b = [0u8; 4];
                    lnames.extend_from_slice(l.encode_utf8(&mut b).as_bytes());
                }
                _ => {
                    let mut b = [0u8; 4];
                    lnames.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                }
            }
        }
    }
    names.push(0);
    lnames.push(0);
    (off, name.len().min(u16::MAX as usize) as u16)
}

/// Lowercase fold used for needles; must match [`push_name`].
pub fn fold(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut dummy = Vec::new();
    push_name(&mut dummy, &mut out, s);
    out.pop();
    String::from_utf8(out).unwrap_or_default()
}

pub struct VolumeIndex {
    pub kind: VolumeKind,
    /// `C:\` for NTFS volumes, the walked folder for `Walk`.
    pub root: String,
    pub entries: Vec<Entry>,
    pub names: Vec<u8>,
    pub lnames: Vec<u8>,
    /// MFT record number -> entry id (Mft volumes only).
    pub slot: Vec<u32>,
    pub usn: Option<UsnPosition>,
    /// Walk volumes: directory id -> child ids (for re-listing a changed directory).
    pub children: HashMap<u32, Vec<u32>>,
    pub live: usize,
    pub garbage: usize,
}

impl VolumeIndex {
    pub fn new(kind: VolumeKind, root: &str) -> VolumeIndex {
        let mut v = VolumeIndex {
            kind,
            root: root.to_string(),
            entries: Vec::new(),
            names: Vec::new(),
            lnames: Vec::new(),
            slot: Vec::new(),
            usn: None,
            children: HashMap::new(),
            live: 0,
            garbage: 0,
        };
        let r = v.root_display().to_string();
        let (off, len) = push_name(&mut v.names, &mut v.lnames, &r);
        v.entries.push(Entry {
            parent: NONE,
            name_off: off,
            size: 0,
            mtime: 0,
            name_len: len,
            flags: F_DIR,
            _pad: 0,
        });
        v.live = 1;
        v
    }

    /// Root name without a trailing separator (`C:`, `D:\Games`).
    pub fn root_display(&self) -> &str {
        let r = self.root.trim_end_matches('\\');
        if r.is_empty() { &self.root } else { r }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn name_bytes(&self, id: u32) -> &[u8] {
        let e = &self.entries[id as usize];
        let s = e.name_off as usize;
        self.names.get(s..s + e.name_len as usize).unwrap_or(&[])
    }

    pub fn name(&self, id: u32) -> &str {
        std::str::from_utf8(self.name_bytes(id)).unwrap_or("?")
    }

    pub fn lname(&self, id: u32) -> &str {
        let e = &self.entries[id as usize];
        let s = e.name_off as usize;
        std::str::from_utf8(self.lnames.get(s..s + e.name_len as usize).unwrap_or(&[])).unwrap_or("")
    }

    /// Full path of an entry (`C:\dir\file.txt`).
    pub fn path(&self, id: u32) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(12);
        let mut cur = id;
        let mut guard = 0;
        while cur != NONE && guard < 1024 {
            parts.push(self.name(cur));
            cur = self.entries[cur as usize].parent;
            guard += 1;
        }
        let mut out = String::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(p);
        }
        if parts.len() == 1 && self.entries[id as usize].parent == NONE && out.ends_with(':') {
            out.push('\\');
        }
        out
    }

    pub fn parent_path(&self, id: u32) -> String {
        let p = self.entries[id as usize].parent;
        if p == NONE { String::new() } else { self.path(p) }
    }

    /// Appends a new entry. `rec` registers it in the MFT slot table (Mft volumes).
    pub fn add(&mut self, rec: Option<u64>, parent: u32, name: &str, flags: u8, size: u64, mtime: u32) -> u32 {
        let id = self.entries.len() as u32;
        let (off, len) = push_name(&mut self.names, &mut self.lnames, name);
        self.entries.push(Entry {
            parent,
            name_off: off,
            size,
            mtime,
            name_len: len,
            flags,
            _pad: 0,
        });
        self.live += 1;
        if let Some(r) = rec {
            self.set_slot(r, id);
        }
        if self.kind == VolumeKind::Walk && parent != NONE {
            self.children.entry(parent).or_default().push(id);
        }
        id
    }

    pub fn set_slot(&mut self, rec: u64, id: u32) {
        let r = rec as usize;
        if r >= self.slot.len() {
            self.slot.resize(r + 1, NONE);
        }
        self.slot[r] = id;
    }

    pub fn id_of_rec(&self, rec: u64) -> Option<u32> {
        self.slot.get(rec as usize).copied().filter(|&i| i != NONE)
    }

    pub fn remove(&mut self, id: u32) {
        let e = &mut self.entries[id as usize];
        if e.flags & F_DELETED != 0 || id == 0 {
            return;
        }
        e.flags |= F_DELETED;
        self.garbage += e.name_len as usize + 1;
        self.live = self.live.saturating_sub(1);
        let parent = e.parent;
        if self.kind == VolumeKind::Walk {
            if let Some(c) = self.children.get_mut(&parent) {
                c.retain(|&x| x != id);
            }
            self.children.remove(&id);
        }
    }

    /// Changes name and/or parent of an existing entry.
    pub fn rename(&mut self, id: u32, new_parent: u32, new_name: &str) {
        let old = &self.entries[id as usize];
        let (old_parent, same_name) = (old.parent, self.name(id) == new_name);
        if old_parent == new_parent && same_name {
            return;
        }
        if !same_name {
            self.garbage += self.entries[id as usize].name_len as usize + 1;
            let (off, len) = push_name(&mut self.names, &mut self.lnames, new_name);
            let e = &mut self.entries[id as usize];
            e.name_off = off;
            e.name_len = len;
        }
        if old_parent != new_parent {
            self.entries[id as usize].parent = new_parent;
            if self.kind == VolumeKind::Walk {
                if let Some(c) = self.children.get_mut(&old_parent) {
                    c.retain(|&x| x != id);
                }
                self.children.entry(new_parent).or_default().push(id);
            }
        }
    }

    pub fn set_stat(&mut self, id: u32, size: u64, mtime: u32) {
        let e = &mut self.entries[id as usize];
        e.size = size;
        e.mtime = mtime;
    }

    /// Rebuilds `children` for Walk volumes (after load or build).
    pub fn rebuild_children(&mut self) {
        self.children.clear();
        if self.kind != VolumeKind::Walk {
            return;
        }
        for (i, e) in self.entries.iter().enumerate() {
            if e.parent != NONE && !e.is_deleted() {
                self.children.entry(e.parent).or_default().push(i as u32);
            }
        }
    }

    /// Walk volumes: finds a directory by absolute path below the root.
    pub fn find_dir(&self, path: &str) -> Option<u32> {
        let root = self.root_display().to_lowercase();
        let p = path.trim_end_matches('\\').to_lowercase();
        let rest = p.strip_prefix(&root)?;
        let mut cur = 0u32;
        for comp in rest.split('\\').filter(|c| !c.is_empty()) {
            let next = self
                .children
                .get(&cur)?
                .iter()
                .copied()
                .find(|&c| self.entries[c as usize].is_dir() && self.lname(c) == comp)?;
            cur = next;
        }
        Some(cur)
    }

    /// Drops garbage (deleted entries, stale names) when it dominates. Ids are renumbered, so callers must hold
    /// the write lock and discard any ids they kept.
    pub fn compact_if_needed(&mut self) -> bool {
        if self.garbage < 4 << 20 || self.garbage * 4 < self.names.len() {
            return false;
        }
        let mut remap = vec![NONE; self.entries.len()];
        let mut entries = Vec::with_capacity(self.live);
        let (mut names, mut lnames) = (Vec::new(), Vec::new());
        for (i, e) in self.entries.iter().enumerate() {
            if e.is_deleted() {
                continue;
            }
            remap[i] = entries.len() as u32;
            let s = e.name_off as usize;
            let n = e.name_len as usize;
            let off = names.len() as u32;
            names.extend_from_slice(&self.names[s..s + n]);
            names.push(0);
            lnames.extend_from_slice(&self.lnames[s..s + n]);
            lnames.push(0);
            entries.push(Entry { name_off: off, ..*e });
        }
        for e in &mut entries {
            if e.parent != NONE {
                e.parent = remap[e.parent as usize];
            }
        }
        for s in &mut self.slot {
            if *s != NONE {
                *s = remap[*s as usize];
            }
        }
        self.entries = entries;
        self.names = names;
        self.lnames = lnames;
        self.garbage = 0;
        self.live = self.entries.len();
        self.rebuild_children();
        true
    }

    pub fn memory_bytes(&self) -> usize {
        self.entries.len() * std::mem::size_of::<Entry>() + self.names.len() * 2 + self.slot.len() * 4
    }
}

#[derive(Default)]
pub struct Index {
    pub volumes: Vec<VolumeIndex>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.volumes.iter().map(VolumeIndex::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replaces the volume with the same root, or adds a new one.
    pub fn set_volume(&mut self, v: VolumeIndex) {
        let key = v.root.to_lowercase();
        match self.volumes.iter().position(|x| x.root.to_lowercase() == key) {
            Some(i) => self.volumes[i] = v,
            None => self.volumes.push(v),
        }
    }

    pub fn volume_for_root(&self, root: &str) -> Option<usize> {
        let key = root.to_lowercase();
        self.volumes.iter().position(|x| x.root.to_lowercase() == key)
    }
}

pub type SharedIndex = Arc<RwLock<Index>>;

pub fn shared() -> SharedIndex {
    Arc::new(RwLock::new(Index::default()))
}

/// Windows FILETIME (100 ns since 1601) -> Unix seconds, clamped to `u32`.
pub fn filetime_to_unix(ft: i64) -> u32 {
    (ft / 10_000_000 - 11_644_473_600).clamp(0, u32::MAX as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_twin_has_identical_offsets() {
        let (mut n, mut l) = (Vec::new(), Vec::new());
        for s in ["Привет.TXT", "ÄÖÜ ß İstanbul", "plain", "ǅ"] {
            let (off, len) = push_name(&mut n, &mut l, s);
            assert_eq!(&n[off as usize..off as usize + len as usize], s.as_bytes());
            assert_eq!(n.len(), l.len());
        }
        assert_eq!(fold("Привет.TXT"), "привет.txt");
    }

    #[test]
    fn paths_rename_remove_and_compact() {
        let mut v = VolumeIndex::new(VolumeKind::Walk, r"D:\");
        let d = v.add(None, 0, "Games", F_DIR, 0, 0);
        let f = v.add(None, d, "Doom.exe", 0, 100, 5);
        assert_eq!(v.path(f), r"D:\Games\Doom.exe");
        assert_eq!(v.path(0), r"D:\");
        assert_eq!(v.find_dir(r"d:\games"), Some(d));
        v.rename(f, 0, "doom2.exe");
        assert_eq!(v.path(f), r"D:\doom2.exe");
        assert_eq!(v.lname(f), "doom2.exe");
        v.remove(f);
        assert_eq!(v.len(), 2);
        v.garbage = 10 << 20;
        v.names.resize(v.names.len().max(10 << 20), 0);
        v.lnames.resize(v.names.len(), 0);
        assert!(v.compact_if_needed());
        assert_eq!(v.len(), 2);
        assert_eq!(v.path(1), r"D:\Games");
    }
}
