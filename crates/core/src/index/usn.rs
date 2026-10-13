//! Keeps an NTFS [`VolumeIndex`] fresh by replaying the USN change journal.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use super::{F_DIR, NONE, SharedIndex, UsnPosition, VolumeIndex, VolumeKind};
use crate::winapi;

const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const REASON_FILE_CREATE: u32 = 0x100;
const REASON_FILE_DELETE: u32 = 0x200;
const REASON_RENAME_OLD: u32 = 0x1000;
const REASON_RENAME_NEW: u32 = 0x2000;
const ATTR_DIR: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;
const FIRST_USER_RECORD: u64 = 24;
const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub frn: u64,
    pub parent: u64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Parses the buffer returned by `FSCTL_READ_USN_JOURNAL`: next USN, then `USN_RECORD_V2` entries.
pub fn parse_records(buf: &[u8]) -> (i64, Vec<UsnRecord>) {
    if buf.len() < 8 {
        return (0, Vec::new());
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
    let mut out = Vec::new();
    let mut off = 8usize;
    while off + 60 <= buf.len() {
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if len < 60 || off + len > buf.len() {
            break;
        }
        let r = &buf[off..off + len];
        let major = u16::from_le_bytes([r[4], r[5]]);
        if major == 2 {
            let u64_at = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap());
            let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
            let name_len = u16::from_le_bytes([r[56], r[57]]) as usize;
            let name_off = u16::from_le_bytes([r[58], r[59]]) as usize;
            if name_off + name_len <= len {
                let units = r[name_off..name_off + name_len].as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p));
                out.push(UsnRecord {
                    frn: u64_at(8) & FRN_MASK,
                    parent: u64_at(16) & FRN_MASK,
                    reason: u32_at(40),
                    attrs: u32_at(52),
                    name: char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect(),
                });
            }
        }
        off += len.next_multiple_of(8);
    }
    (next, out)
}

/// Structural changes of one batch. Returns the ids whose size/mtime must be refreshed.
pub fn apply_structure(vol: &mut VolumeIndex, recs: &[UsnRecord]) -> Vec<u32> {
    let mut touched: Vec<u32> = Vec::new();
    for r in recs {
        if r.frn < FIRST_USER_RECORD && r.frn != 5 {
            continue;
        }
        if r.reason & REASON_FILE_DELETE != 0 {
            if let Some(id) = vol.id_of_rec(r.frn) {
                vol.remove(id);
                vol.slot[r.frn as usize] = NONE;
            }
            continue;
        }
        // A rename arrives as OLD_NAME (stale name) followed by NEW_NAME; only the latter is trusted.
        if r.reason & REASON_RENAME_OLD != 0 && r.reason & REASON_RENAME_NEW == 0 {
            continue;
        }
        let existing = vol.id_of_rec(r.frn);
        let structural = r.reason & (REASON_FILE_CREATE | REASON_RENAME_NEW) != 0 || existing.is_none();
        if r.name.is_empty() {
            continue;
        }
        if structural {
            let Some(parent) = (if r.parent == 5 { Some(0) } else { vol.id_of_rec(r.parent) }) else {
                continue; // parent unknown (created in a metafile folder, or a gap): ignore
            };
            match existing {
                Some(id) => vol.rename(id, parent, &r.name),
                None => {
                    let flags = (if r.attrs & ATTR_DIR != 0 { F_DIR } else { 0 })
                        | (if r.attrs & ATTR_REPARSE != 0 { super::F_REPARSE } else { 0 });
                    let id = vol.add(Some(r.frn), parent, &r.name, flags, 0, 0);
                    touched.push(id);
                }
            }
        }
        if let Some(id) = vol.id_of_rec(r.frn) {
            touched.push(id);
        }
    }
    touched.sort_unstable();
    touched.dedup();
    touched
}

pub enum Outcome {
    Stopped,
    /// The journal was recreated or wrapped around: the volume must be scanned again.
    NeedsRescan(String),
}

/// Replays journal records until `stop` is set. `root` identifies the volume inside the shared index.
pub fn follow(drive: char, root: &str, index: &SharedIndex, stop: &AtomicBool, on_change: &dyn Fn()) -> Outcome {
    let vol_file = match winapi::open_volume(drive) {
        Ok(f) => f,
        Err(e) => return Outcome::NeedsRescan(format!("не удалось открыть том {drive}: {e}")),
    };
    let journal = match winapi::query_usn_journal(&vol_file) {
        Ok(j) => j,
        Err(e) => return Outcome::NeedsRescan(format!("журнал USN недоступен: {e}")),
    };
    let mut pos = {
        let g = index.read();
        let Some(i) = g.volume_for_root(root) else {
            return Outcome::NeedsRescan("том отсутствует в индексе".into());
        };
        match g.volumes[i].usn {
            Some(u) if u.journal_id == journal.id && u.next_usn >= journal.lowest_valid => u,
            _ => return Outcome::NeedsRescan("журнал USN пересоздан или переполнен".into()),
        }
    };
    let mut buf = vec![0u8; 256 * 1024];
    while !stop.load(Relaxed) {
        let n = match winapi::read_usn_journal(&vol_file, pos.journal_id, pos.next_usn, 1, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                return match e.raw_os_error() {
                    Some(ERROR_JOURNAL_ENTRY_DELETED | ERROR_JOURNAL_DELETE_IN_PROGRESS | ERROR_JOURNAL_NOT_ACTIVE) => {
                        Outcome::NeedsRescan("записи журнала USN были удалены".into())
                    }
                    _ => Outcome::NeedsRescan(format!("чтение журнала USN: {e}")),
                };
            }
        };
        if n < 8 {
            continue;
        }
        let (next, recs) = parse_records(&buf[..n]);
        if recs.is_empty() {
            pos.next_usn = next.max(pos.next_usn);
            continue;
        }
        let touched = {
            let mut g = index.write();
            let Some(i) = g.volume_for_root(root) else {
                return Outcome::NeedsRescan("том исчез из индекса".into());
            };
            let v = &mut g.volumes[i];
            if v.kind != VolumeKind::Mft {
                return Outcome::Stopped;
            }
            let t = apply_structure(v, &recs);
            v.usn = Some(UsnPosition { journal_id: pos.journal_id, next_usn: next });
            t.into_iter().map(|id| (id, v.path(id))).collect::<Vec<_>>()
        };
        // File system calls happen without holding the lock.
        let stats: Vec<(u32, Option<(u64, u32)>)> = touched.iter().map(|(id, p)| (*id, stat(p))).collect();
        {
            let mut g = index.write();
            if let Some(i) = g.volume_for_root(root) {
                let v = &mut g.volumes[i];
                for (id, st) in stats {
                    match st {
                        Some((size, mtime)) => {
                            let is_dir = v.entries[id as usize].is_dir();
                            v.set_stat(id, if is_dir { 0 } else { size }, mtime);
                        }
                        None => v.remove(id),
                    }
                }
                v.compact_if_needed();
            }
        }
        pos.next_usn = next;
        on_change();
    }
    Outcome::Stopped
}

/// `(size, mtime)` of a path, `None` if it does not exist.
pub fn stat(path: &str) -> Option<(u64, u32)> {
    let m = std::fs::symlink_metadata(winapi::long_path(Path::new(path))).ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32);
    Some((m.len(), mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_bytes(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let nb: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let len = (60 + nb.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&(frn | 7 << 48).to_le_bytes());
        r[16..24].copy_from_slice(&(parent | 1 << 48).to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(nb.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + nb.len()].copy_from_slice(&nb);
        r
    }

    fn buffer(recs: &[Vec<u8>]) -> Vec<u8> {
        let mut b = 1234i64.to_le_bytes().to_vec();
        for r in recs {
            b.extend_from_slice(r);
        }
        b
    }

    #[test]
    fn parses_records() {
        let b = buffer(&[rec_bytes(100, 5, REASON_FILE_CREATE, 0x20, "Файл.txt"), rec_bytes(101, 100, 0x2, 0x10, "d")]);
        let (next, r) = parse_records(&b);
        assert_eq!(next, 1234);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], UsnRecord { frn: 100, parent: 5, reason: REASON_FILE_CREATE, attrs: 0x20, name: "Файл.txt".into() });
        assert_eq!(r[1].frn, 101);
        // truncated buffers do not panic
        for cut in 0..b.len() {
            let _ = parse_records(&b[..cut]);
        }
    }

    #[test]
    fn create_rename_delete_cycle() {
        let mut v = VolumeIndex::new(VolumeKind::Mft, r"C:\");
        v.set_slot(5, 0);
        let mk = |frn, parent, reason, attrs, name: &str| UsnRecord { frn, parent, reason, attrs, name: name.into() };
        // create dir 100 in root, file 101 in dir 100
        let t = apply_structure(
            &mut v,
            &[mk(100, 5, REASON_FILE_CREATE, ATTR_DIR, "Dir"), mk(101, 100, REASON_FILE_CREATE, 0x20, "a.txt")],
        );
        assert_eq!(t.len(), 2);
        let f = v.id_of_rec(101).unwrap();
        assert_eq!(v.path(f), r"C:\Dir\a.txt");
        // rename: old-name record is ignored, new-name record applies
        apply_structure(
            &mut v,
            &[mk(101, 100, REASON_RENAME_OLD, 0x20, "a.txt"), mk(101, 5, REASON_RENAME_NEW, 0x20, "Б.txt")],
        );
        assert_eq!(v.path(f), r"C:\Б.txt");
        assert_eq!(v.lname(f), "б.txt");
        // data change only touches
        let t = apply_structure(&mut v, &[mk(101, 5, 0x2, 0x20, "Б.txt")]);
        assert_eq!(t, vec![f]);
        // delete
        apply_structure(&mut v, &[mk(101, 5, REASON_FILE_DELETE, 0x20, "Б.txt")]);
        assert!(v.id_of_rec(101).is_none());
        assert!(v.entries[f as usize].is_deleted());
        // unknown parent and metafile records are ignored
        apply_structure(&mut v, &[mk(300, 999, REASON_FILE_CREATE, 0x20, "x"), mk(11, 5, REASON_FILE_CREATE, 0x20, "$m")]);
        assert!(v.id_of_rec(300).is_none() && v.id_of_rec(11).is_none());
    }
}
