//! Index persistence: one binary file, checksummed, written atomically (temp file + rename). Loading validates
//! every offset, so a truncated or tampered file is rejected instead of trusted.

use std::path::Path;

use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use super::{Entry, Index, NONE, UsnPosition, VolumeIndex, VolumeKind};
use crate::error::{Error, Result};

const MAGIC: &[u8; 8] = b"STRELAIX";
const VERSION: u32 = 1;
const ENTRY_BYTES: usize = 24;

fn encode_entries(entries: &[Entry]) -> Vec<u8> {
    let mut out = vec![0u8; entries.len() * ENTRY_BYTES];
    out.par_chunks_mut(ENTRY_BYTES * 65536)
        .zip(entries.par_chunks(65536))
        .for_each(|(dst, src)| {
            for (d, e) in dst.as_chunks_mut::<ENTRY_BYTES>().0.iter_mut().zip(src) {
                d[0..4].copy_from_slice(&e.parent.to_le_bytes());
                d[4..8].copy_from_slice(&e.name_off.to_le_bytes());
                d[8..16].copy_from_slice(&e.size.to_le_bytes());
                d[16..20].copy_from_slice(&e.mtime.to_le_bytes());
                d[20..22].copy_from_slice(&e.name_len.to_le_bytes());
                d[22] = e.flags;
                d[23] = 0;
            }
        });
    out
}

/// Serialises the whole index (checksum included). Fast: parallel copy, no I/O — safe to run under a read lock.
pub fn encode(index: &Index) -> Vec<u8> {
    let cap: usize = index
        .volumes
        .iter()
        .map(|v| v.entries.len() * ENTRY_BYTES + v.names.len() + v.slot.len() * 4 + 128)
        .sum::<usize>()
        + 64;
    let mut w: Vec<u8> = Vec::with_capacity(cap);
    w.extend_from_slice(MAGIC);
    w.extend_from_slice(&VERSION.to_le_bytes());
    w.extend_from_slice(&(index.volumes.len() as u32).to_le_bytes());
    for v in &index.volumes {
        w.push(match v.kind {
            VolumeKind::Mft => 0u8,
            VolumeKind::Walk => 1,
        });
        w.extend_from_slice(&(v.root.len() as u16).to_le_bytes());
        w.extend_from_slice(v.root.as_bytes());
        let usn = v.usn.unwrap_or_default();
        w.push(v.usn.is_some() as u8);
        w.extend_from_slice(&usn.journal_id.to_le_bytes());
        w.extend_from_slice(&usn.next_usn.to_le_bytes());
        w.extend_from_slice(&(v.entries.len() as u64).to_le_bytes());
        w.extend_from_slice(&(v.names.len() as u64).to_le_bytes());
        w.extend_from_slice(&(v.slot.len() as u64).to_le_bytes());
        w.extend_from_slice(&encode_entries(&v.entries));
        w.extend_from_slice(&v.names);
        for s in &v.slot {
            w.extend_from_slice(&s.to_le_bytes());
        }
    }
    let sum = xxh3_64(&w);
    w.extend_from_slice(&sum.to_le_bytes());
    w
}

/// Writes atomically: temp file next to the target, then rename over it.
pub fn write_atomic(bytes: &[u8], path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save(index: &Index, path: &Path) -> Result<()> {
    write_atomic(&encode(index), path)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or_else(|| bad("файл индекса обрезан"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn bad(msg: &str) -> Error {
    Error::Unsupported(msg.to_string())
}

/// Lowercase twin of a names arena (same lengths, NUL separators preserved).
pub fn fold_arena(names: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0u8; names.len()];
    // Split at NUL separators so no character is cut in half.
    let mut cuts = vec![0usize];
    let step = (names.len() / 256).max(1 << 16);
    let mut p = step;
    while p < names.len() {
        match names[p..].iter().position(|&b| b == 0) {
            Some(i) => {
                p += i + 1;
                cuts.push(p.min(names.len()));
            }
            None => break,
        }
        p += step;
    }
    cuts.push(names.len());
    cuts.dedup();
    let mut parts: Vec<(&[u8], &mut [u8])> = Vec::with_capacity(cuts.len());
    let mut rest: &mut [u8] = &mut out;
    for w in cuts.windows(2) {
        let (head, tail) = rest.split_at_mut(w[1] - w[0]);
        parts.push((&names[w[0]..w[1]], head));
        rest = tail;
    }
    let ok = parts.into_par_iter().all(|(src, dst)| {
        if src.is_ascii() {
            for (d, s) in dst.iter_mut().zip(src) {
                *d = s.to_ascii_lowercase();
            }
            return true;
        }
        let Ok(text) = std::str::from_utf8(src) else { return false };
        let mut at = 0;
        for ch in text.chars() {
            let mut lower = ch.to_lowercase();
            let mapped = match (lower.next(), lower.next()) {
                (Some(l), None) if l.len_utf8() == ch.len_utf8() => l,
                _ => ch,
            };
            at += mapped.encode_utf8(&mut dst[at..]).len();
        }
        true
    });
    if ok { Ok(out) } else { Err(bad("имена в индексе не UTF-8")) }
}

pub fn load(path: &Path) -> Result<Index> {
    let data = std::fs::read(path)?;
    if data.len() < 8 + 4 + 4 + 8 {
        return Err(bad("файл индекса слишком мал"));
    }
    let (body, tail) = data.split_at(data.len() - 8);
    if xxh3_64(body) != u64::from_le_bytes(tail.try_into().unwrap()) {
        return Err(bad("контрольная сумма индекса не сходится"));
    }
    let mut r = Reader { b: body, pos: 0 };
    if r.take(8)? != MAGIC {
        return Err(bad("это не файл индекса Strela"));
    }
    if r.u32()? != VERSION {
        return Err(bad("неизвестная версия индекса"));
    }
    let vols = r.u32()? as usize;
    if vols > 64 {
        return Err(bad("слишком много томов"));
    }
    let mut index = Index::default();
    for _ in 0..vols {
        let kind = match r.u8()? {
            0 => VolumeKind::Mft,
            1 => VolumeKind::Walk,
            _ => return Err(bad("неизвестный тип тома")),
        };
        let rl = r.u16()? as usize;
        let root = String::from_utf8(r.take(rl)?.to_vec()).map_err(|_| bad("корень тома не UTF-8"))?;
        let has_usn = r.u8()? != 0;
        let (jid, next) = (r.u64()?, r.u64()? as i64);
        let (ne, nn, ns) = (r.u64()? as usize, r.u64()? as usize, r.u64()? as usize);
        if ne == 0 || ne >= NONE as usize || nn >= u32::MAX as usize || ns > u32::MAX as usize {
            return Err(bad("недопустимые размеры индекса"));
        }
        let eb = r.take(ne.checked_mul(ENTRY_BYTES).ok_or_else(|| bad("переполнение"))?)?;
        let names = r.take(nn)?.to_vec();
        let sb = r.take(ns.checked_mul(4).ok_or_else(|| bad("переполнение"))?)?;

        let entries: Vec<Entry> = eb
            .par_chunks_exact(ENTRY_BYTES)
            .map(|d| Entry {
                parent: u32::from_le_bytes(d[0..4].try_into().unwrap()),
                name_off: u32::from_le_bytes(d[4..8].try_into().unwrap()),
                size: u64::from_le_bytes(d[8..16].try_into().unwrap()),
                mtime: u32::from_le_bytes(d[16..20].try_into().unwrap()),
                name_len: u16::from_le_bytes(d[20..22].try_into().unwrap()),
                flags: d[22],
                _pad: 0,
            })
            .collect();
        let valid = entries.par_iter().all(|e| {
            (e.parent == NONE || (e.parent as usize) < ne)
                && e.name_off as usize + e.name_len as usize <= nn
                && std::str::from_utf8(&names[e.name_off as usize..e.name_off as usize + e.name_len as usize]).is_ok()
        });
        if !valid {
            return Err(bad("в индексе недопустимые ссылки"));
        }
        let slot: Vec<u32> = sb.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        if slot.iter().any(|&s| s != NONE && s as usize >= ne) {
            return Err(bad("в индексе недопустимая таблица записей"));
        }
        let lnames = fold_arena(&names)?;
        let live = entries.par_iter().filter(|e| !e.is_deleted()).count();
        let mut v = VolumeIndex::new(kind, &root);
        v.entries = entries;
        v.names = names;
        v.lnames = lnames;
        v.slot = slot;
        v.usn = has_usn.then_some(UsnPosition { journal_id: jid, next_usn: next });
        v.live = live;
        v.garbage = 0;
        v.rebuild_children();
        index.volumes.push(v);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{F_DIR, VolumeIndex};

    fn sample() -> Index {
        let mut v = VolumeIndex::new(VolumeKind::Walk, r"D:\");
        let d = v.add(None, 0, "Игры", F_DIR, 0, 1);
        v.add(None, d, "Doom.EXE", 0, 12345, 99);
        v.usn = Some(UsnPosition { journal_id: 7, next_usn: 42 });
        let mut i = Index::default();
        i.volumes.push(v);
        i
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("i.bin");
        save(&sample(), &p).unwrap();
        let back = load(&p).unwrap();
        let v = &back.volumes[0];
        assert_eq!(v.len(), 3);
        assert_eq!(v.path(2), r"D:\Игры\Doom.EXE");
        assert_eq!(v.lname(2), "doom.exe");
        assert_eq!(v.usn, Some(UsnPosition { journal_id: 7, next_usn: 42 }));
        assert_eq!(v.entries[2].size, 12345);
        assert_eq!(v.children[&1], vec![2]);
    }

    #[test]
    fn corruption_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("i.bin");
        save(&sample(), &p).unwrap();
        let mut b = std::fs::read(&p).unwrap();
        let n = b.len();
        b[n / 2] ^= 0xFF;
        std::fs::write(&p, &b).unwrap();
        assert!(load(&p).is_err());
        std::fs::write(&p, &b[..n / 3]).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn tampered_but_checksummed_file_is_rejected() {
        // a valid checksum over bogus offsets must still fail validation
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("i.bin");
        save(&sample(), &p).unwrap();
        let mut b = std::fs::read(&p).unwrap();
        let n = b.len();
        // header: 8+4+4 + 1+2+3(root "D:\") +1+8+8+8+8+8 = 55; first entry starts at 55; set its name_off huge
        b[55 + 4..55 + 8].copy_from_slice(&0x7FFF_0000u32.to_le_bytes());
        let sum = xxh3_64(&b[..n - 8]);
        b[n - 8..].copy_from_slice(&sum.to_le_bytes());
        std::fs::write(&p, &b).unwrap();
        assert!(load(&p).is_err());
    }
}
