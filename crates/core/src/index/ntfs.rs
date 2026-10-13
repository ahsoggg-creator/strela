//! Platform-independent NTFS `$MFT` record parser and index builder.
//!
//! Format references (offsets verified against these pages):
//! - FILE record header / flags: <https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html>
//! - Fixup (update sequence): <https://flatcap.github.io/linux-ntfs/ntfs/concepts/fixup.html>
//! - Attribute header: <https://flatcap.github.io/linux-ntfs/ntfs/concepts/attribute_header.html>
//! - `$STANDARD_INFORMATION`: <https://flatcap.github.io/linux-ntfs/ntfs/attributes/standard_information.html>
//! - `$FILE_NAME`: <https://flatcap.github.io/linux-ntfs/ntfs/attributes/file_name.html>
//! - Data runs: <https://flatcap.github.io/linux-ntfs/ntfs/concepts/data_runs.html>
//! - `$ATTRIBUTE_LIST`: <https://flatcap.github.io/linux-ntfs/ntfs/attributes/attribute_list.html>

use rayon::prelude::*;

use super::{Entry, F_DELETED, F_DIR, F_REPARSE, NONE, VolumeIndex, VolumeKind, filetime_to_unix, push_name};
use crate::error::{Error, Result};

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FLAG_IN_USE: u16 = 0x01;
const FLAG_DIR: u16 = 0x02;
const NS_DOS: u8 = 2;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// MFT record number of the root directory (`.`).
pub const ROOT_RECORD: u32 = 5;
/// Records below this number are NTFS metafiles (`$MFT`, `$LogFile`, ...), never indexed.
const FIRST_USER_RECORD: u32 = 24;
const FIXUP_STRIDE: usize = 512;
const DOS_REPARSE: u32 = 0x400;

const B_DIR: u8 = 1;
const B_HAS_NAME: u8 = 2;
const B_HAS_DATA: u8 = 4;
const B_REPARSE: u8 = 8;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Undoes the multi-sector transfer protection. Returns false for torn/corrupt records.
pub fn apply_fixups(rec: &mut [u8]) -> bool {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16_at(rec, 4) as usize;
    let usa_cnt = u16_at(rec, 6) as usize;
    if usa_cnt == 0 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * FIXUP_STRIDE > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * FIXUP_STRIDE;
        if rec[end - 2..end] != usn {
            return false;
        }
        rec[end - 2] = rec[usa_off + 2 * i];
        rec[end - 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

#[derive(Clone, Copy, Debug)]
pub struct BaseRec {
    pub rec: u32,
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u8,
    pub size: u64,
    pub mtime: u32,
}

/// Attributes found in an extension record that belong to `base`.
#[derive(Clone, Copy, Debug)]
pub struct ExtRec {
    pub base: u32,
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u8,
    pub size: u64,
}

#[derive(Default)]
pub struct ChunkOut {
    pub bases: Vec<BaseRec>,
    pub exts: Vec<ExtRec>,
    pub names: Vec<u8>,
    pub lnames: Vec<u8>,
}

#[derive(Default)]
struct Found {
    parent: u32,
    name_off: u32,
    name_len: u16,
    has_name: bool,
    size: u64,
    has_data: bool,
    mtime: u32,
    reparse: bool,
}

/// Parses one FILE record (fixups are applied in place). `rec_no` is the record's index in `$MFT`.
pub fn parse_record(buf: &mut [u8], rec_no: u32, out: &mut ChunkOut) {
    // Free records are the majority on many volumes; the flags word lies before the first sector tail,
    // so it can be checked before (and instead of) applying the fixups.
    if buf.len() < 0x30 || &buf[0..4] != b"FILE" || u16_at(buf, 0x16) & FLAG_IN_USE == 0 {
        return;
    }
    if !apply_fixups(buf) {
        return;
    }
    let flags = u16_at(buf, 0x16);
    let base_ref = u64_at(buf, 0x20) & FRN_MASK;
    let used = (u32_at(buf, 0x18) as usize).min(buf.len());
    let mut off = u16_at(buf, 0x14) as usize;
    let mut f = Found::default();

    while off + 8 <= used {
        let ty = u32_at(buf, off);
        // Attributes are stored in ascending type order; nothing after $DATA is needed.
        if ty == ATTR_END || ty > ATTR_DATA {
            break;
        }
        let len = u32_at(buf, off + 4) as usize;
        if len < 0x10 || !len.is_multiple_of(8) || off + len > buf.len() {
            break;
        }
        let attr = &buf[off..off + len];
        match ty {
            ATTR_STANDARD_INFORMATION if attr[8] == 0 => parse_standard_info(attr, &mut f),
            ATTR_FILE_NAME if attr[8] == 0 => parse_file_name(attr, out, &mut f),
            ATTR_DATA if attr[9] == 0 => parse_data(attr, &mut f),
            _ => {}
        }
        off += len;
    }

    let mut bflags = 0;
    if flags & FLAG_DIR != 0 {
        bflags |= B_DIR;
    }
    if f.has_name {
        bflags |= B_HAS_NAME;
    }
    if f.has_data {
        bflags |= B_HAS_DATA;
    }
    if f.reparse {
        bflags |= B_REPARSE;
    }
    if base_ref == 0 {
        out.bases.push(BaseRec {
            rec: rec_no,
            parent: f.parent,
            name_off: f.name_off,
            name_len: f.name_len,
            flags: bflags,
            size: f.size,
            mtime: f.mtime,
        });
    } else if (f.has_name || f.has_data) && base_ref <= u32::MAX as u64 {
        out.exts.push(ExtRec {
            base: base_ref as u32,
            parent: f.parent,
            name_off: f.name_off,
            name_len: f.name_len,
            flags: bflags,
            size: f.size,
        });
    }
}

/// Resident `$STANDARD_INFORMATION`: "file altered" time (content +0x08) and DOS attributes (+0x20).
fn parse_standard_info(attr: &[u8], f: &mut Found) {
    if attr.len() < 0x18 {
        return;
    }
    let coff = u16_at(attr, 0x14) as usize;
    if u32_at(attr, 0x10) < 0x10 || coff + 0x10 > attr.len() {
        return;
    }
    f.mtime = filetime_to_unix(u64_at(attr, coff + 0x08) as i64);
    if u32_at(attr, 0x10) >= 0x24 && coff + 0x24 <= attr.len() {
        f.reparse = u32_at(attr, coff + 0x20) & DOS_REPARSE != 0;
    }
}

/// Resident `$FILE_NAME`. The first non-DOS name wins; further links are not indexed.
fn parse_file_name(attr: &[u8], out: &mut ChunkOut, f: &mut Found) {
    if attr.len() < 0x18 || f.has_name {
        return;
    }
    let clen = u32_at(attr, 0x10) as usize;
    let coff = u16_at(attr, 0x14) as usize;
    if coff + 0x42 > attr.len() || clen < 0x42 {
        return;
    }
    let c = &attr[coff..];
    let nchars = c[0x40] as usize;
    let ns = c[0x41];
    if ns == NS_DOS || 0x42 + nchars * 2 > c.len() || nchars == 0 {
        return;
    }
    let units = c[0x42..0x42 + nchars * 2].as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p));
    let name: String = char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    let (off, len) = push_name(&mut out.names, &mut out.lnames, &name);
    f.parent = (u64_at(c, 0) & FRN_MASK).min(u32::MAX as u64) as u32;
    f.name_off = off;
    f.name_len = len;
    f.has_name = true;
}

/// Unnamed `$DATA`.
fn parse_data(attr: &[u8], f: &mut Found) {
    if f.has_data {
        return;
    }
    if attr[8] == 0 {
        if attr.len() >= 0x18 {
            f.size = u32_at(attr, 0x10) as u64;
            f.has_data = true;
        }
        return;
    }
    // Non-resident: sizes are only valid in the first extent (starting VCN 0).
    if attr.len() < 0x40 || u64_at(attr, 0x10) != 0 {
        return;
    }
    f.size = u64_at(attr, 0x30);
    f.has_data = true;
}

/// One extent of a non-resident attribute. `lcn == None` is a sparse run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub len: u64,
    pub lcn: Option<u64>,
}

/// Decodes a data-run list (bytes starting at the "data runs offset").
pub fn parse_runs(mut b: &[u8]) -> Result<Vec<Run>> {
    let bad = || Error::Ntfs("повреждённый список data runs".into());
    let (mut runs, mut vcn, mut lcn) = (Vec::new(), 0u64, 0i64);
    while let Some(&hdr) = b.first() {
        if hdr == 0 {
            break;
        }
        let (ls, os) = ((hdr & 0x0F) as usize, (hdr >> 4) as usize);
        if ls == 0 || ls > 8 || os > 8 || b.len() < 1 + ls + os {
            return Err(bad());
        }
        let mut len = 0u64;
        for (i, &x) in b[1..1 + ls].iter().enumerate() {
            len |= (x as u64) << (8 * i);
        }
        let run_lcn = if os == 0 {
            None
        } else {
            let o = &b[1 + ls..1 + ls + os];
            let mut v = 0i64;
            for (i, &x) in o.iter().enumerate() {
                v |= (x as i64) << (8 * i);
            }
            if o[os - 1] & 0x80 != 0 && os < 8 {
                v |= -1i64 << (8 * os);
            }
            lcn = lcn.checked_add(v).ok_or_else(bad)?;
            if lcn < 0 {
                return Err(bad());
            }
            Some(lcn as u64)
        };
        runs.push(Run { vcn, len, lcn: run_lcn });
        vcn += len;
        b = &b[1 + ls + os..];
    }
    Ok(runs)
}

/// Entries of a `$ATTRIBUTE_LIST`: `(attribute type, starting VCN, base record number)`.
pub fn parse_attribute_list(mut b: &[u8]) -> Vec<(u32, u64, u64)> {
    let mut v = Vec::new();
    while b.len() >= 0x1A {
        let elen = u16_at(b, 4) as usize;
        if elen < 0x1A || elen > b.len() {
            break;
        }
        v.push((u32_at(b, 0), u64_at(b, 8), u64_at(b, 0x10) & FRN_MASK));
        b = &b[elen..];
    }
    v
}

/// Locates the attribute `ty` (unnamed) in an already fixed-up record.
pub fn find_attr(rec: &[u8], ty: u32) -> Option<&[u8]> {
    let used = (u32_at(rec, 0x18) as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14) as usize;
    while off + 8 <= used {
        let t = u32_at(rec, off);
        if t == ATTR_END {
            break;
        }
        let len = u32_at(rec, off + 4) as usize;
        if len < 0x10 || !len.is_multiple_of(8) || off + len > rec.len() {
            break;
        }
        if t == ty && rec[off + 9] == 0 {
            return Some(&rec[off..off + len]);
        }
        off += len;
    }
    None
}

/// Merges parsed chunks into a [`VolumeIndex`]. `root` is the volume root (`C:\`).
pub fn build_volume(chunks: Vec<ChunkOut>, root: &str) -> Result<VolumeIndex> {
    let mut chunks = chunks;
    let mut shifts = Vec::with_capacity(chunks.len());
    let (mut name_total, mut base_total, mut ext_total) = (0usize, 0usize, 0usize);
    for c in &chunks {
        shifts.push(name_total as u32);
        name_total += c.names.len();
        base_total += c.bases.len();
        ext_total += c.exts.len();
    }
    chunks.par_iter_mut().zip(shifts).for_each(|(c, shift)| {
        if shift != 0 {
            for b in &mut c.bases {
                b.name_off += shift;
            }
            for e in &mut c.exts {
                e.name_off += shift;
            }
        }
    });
    let mut names: Vec<u8> = Vec::with_capacity(name_total + 512);
    let mut lnames: Vec<u8> = Vec::with_capacity(name_total + 512);
    let mut bases: Vec<BaseRec> = Vec::with_capacity(base_total);
    let mut exts: Vec<ExtRec> = Vec::with_capacity(ext_total);
    for mut c in chunks {
        names.extend_from_slice(&c.names);
        lnames.extend_from_slice(&c.lnames);
        bases.append(&mut c.bases);
        exts.append(&mut c.exts);
    }

    let max_rec = bases.iter().map(|b| b.rec).max().unwrap_or(0) as usize;
    let mut slot = vec![NONE; max_rec + 1];
    for (i, b) in bases.iter().enumerate() {
        slot[b.rec as usize] = i as u32;
    }
    for e in &exts {
        let Some(&i) = slot.get(e.base as usize).filter(|&&i| i != NONE) else {
            continue;
        };
        let b = &mut bases[i as usize];
        if e.flags & B_HAS_NAME != 0 && b.flags & B_HAS_NAME == 0 {
            (b.parent, b.name_off, b.name_len) = (e.parent, e.name_off, e.name_len);
            b.flags |= B_HAS_NAME;
        }
        if e.flags & B_HAS_DATA != 0 && b.flags & B_HAS_DATA == 0 {
            b.size = e.size;
            b.flags |= B_HAS_DATA;
        }
    }

    let root_idx = slot.get(ROOT_RECORD as usize).copied().filter(|&i| i != NONE);
    let Some(root_idx) = root_idx.filter(|&i| bases[i as usize].flags & B_DIR != 0) else {
        return Err(Error::Ntfs("в $MFT не найден корневой каталог (запись 5)".into()));
    };

    // A record is indexed if it has a name and is not an NTFS metafile; the root is entry 0.
    let included = |b: &BaseRec| b.flags & B_HAS_NAME != 0 && b.rec >= FIRST_USER_RECORD;
    let parent_ok = |b: &BaseRec| {
        b.parent == ROOT_RECORD
            || slot
                .get(b.parent as usize)
                .copied()
                .filter(|&i| i != NONE)
                .is_some_and(|i| included(&bases[i as usize]))
    };
    let order: Vec<u32> = std::iter::once(root_idx)
        .chain(
            bases
                .iter()
                .enumerate()
                .filter(|(_, b)| included(b) && b.rec != b.parent && parent_ok(b))
                .map(|(i, _)| i as u32),
        )
        .collect();

    let mut node_of = vec![NONE; max_rec + 1];
    node_of[ROOT_RECORD as usize] = 0;
    for (id, &bi) in order.iter().enumerate().skip(1) {
        node_of[bases[bi as usize].rec as usize] = id as u32;
    }

    let mut vol = VolumeIndex::new(VolumeKind::Mft, root);
    let root_entry = vol.entries[0];
    vol.names = names;
    vol.lnames = lnames;
    let root_name = vol.root_display().to_string();
    let (roff, rlen) = push_name(&mut vol.names, &mut vol.lnames, &root_name);
    let mut entries: Vec<Entry> = Vec::with_capacity(order.len());
    entries.push(Entry {
        name_off: roff,
        name_len: rlen,
        ..root_entry
    });
    entries.par_extend(order[1..].par_iter().with_min_len(8192).map(|&bi| {
        let b = &bases[bi as usize];
        let mut flags = 0;
        if b.flags & B_DIR != 0 {
            flags |= F_DIR;
        }
        if b.flags & B_REPARSE != 0 {
            flags |= F_REPARSE;
        }
        let parent = node_of.get(b.parent as usize).copied().unwrap_or(NONE);
        if parent == NONE {
            // The parent itself was dropped (its own chain is broken): keep the id, hide the entry.
            flags |= F_DELETED;
        }
        Entry {
            parent,
            name_off: b.name_off,
            size: if b.flags & B_DIR != 0 { 0 } else { b.size },
            mtime: b.mtime,
            name_len: b.name_len,
            flags,
            _pad: 0,
        }
    }));
    let hidden = entries.iter().filter(|e| e.flags & F_DELETED != 0).count();
    vol.live = entries.len() - hidden;
    vol.garbage = hidden * 16;
    vol.entries = entries;
    let mut slot_out = vec![NONE; max_rec + 1];
    for (id, &bi) in order.iter().enumerate() {
        slot_out[bases[bi as usize].rec as usize] = id as u32;
    }
    vol.slot = slot_out;
    Ok(vol)
}

#[cfg(test)]
pub mod testutil {
    //! Builder for synthetic FILE records (1024 bytes, 2 sectors, fixups applied).

    pub fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    fn align8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    pub fn file_name_attr(parent: u64, name: &str, ns: u8) -> Vec<u8> {
        let mut c = vec![0u8; 0x42];
        c[0..8].copy_from_slice(&(parent | (1 << 48)).to_le_bytes());
        c[0x40] = name.encode_utf16().count() as u8;
        c[0x41] = ns;
        c.extend(utf16(name));
        resident(0x30, &c)
    }

    pub fn resident(ty: u32, content: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 0x18];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(content.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a.extend_from_slice(content);
        align8(&mut a);
        let l = a.len() as u32;
        a[4..8].copy_from_slice(&l.to_le_bytes());
        a
    }

    pub fn nonresident_data(real: u64, alloc: u64, start_vcn: u64) -> Vec<u8> {
        let mut a = vec![0u8; 0x48];
        a[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[8] = 1;
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x48u16.to_le_bytes());
        a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real.to_le_bytes());
        a.push(0);
        align8(&mut a);
        let l = a.len() as u32;
        a[4..8].copy_from_slice(&l.to_le_bytes());
        a
    }

    pub fn std_info(mtime_unix: u64, dos_attrs: u32) -> Vec<u8> {
        let mut si = vec![0u8; 0x30];
        si[8..16].copy_from_slice(&((mtime_unix + 11_644_473_600) * 10_000_000).to_le_bytes());
        si[0x20..0x24].copy_from_slice(&dos_attrs.to_le_bytes());
        resident(0x10, &si)
    }

    pub fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        off += 8;
        r[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
        r[0x30..0x32].copy_from_slice(&0xBEEFu16.to_le_bytes());
        for i in 1..3 {
            let end = i * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x30 + 2 * i] = a;
            r[0x30 + 2 * i + 1] = b;
            r[end - 2..end].copy_from_slice(&0xBEEFu16.to_le_bytes());
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn parse_all(recs: Vec<Vec<u8>>) -> Vec<ChunkOut> {
        let mut out = ChunkOut::default();
        for (i, mut r) in recs.into_iter().enumerate() {
            parse_record(&mut r, i as u32, &mut out);
        }
        vec![out]
    }

    #[test]
    fn fixups_restore_sector_tails_and_reject_torn_records() {
        let mut r = record(1, 0, &[]);
        r[510] = 0x11;
        r[511] = 0x22;
        assert!(!apply_fixups(&mut r));
        let mut r = record(1, 0, &[]);
        assert!(apply_fixups(&mut r));
        assert_eq!(&r[510..512], &[0, 0]);
    }

    #[test]
    fn decodes_data_runs_with_negative_and_sparse() {
        let b = [0x11, 0x30, 0x60, 0x21, 0x10, 0x00, 0x01, 0x11, 0x20, 0xE0, 0x01, 0x05, 0x00];
        let r = parse_runs(&b).unwrap();
        assert_eq!(r[0], Run { vcn: 0, len: 0x30, lcn: Some(0x60) });
        assert_eq!(r[1], Run { vcn: 0x30, len: 0x10, lcn: Some(0x160) });
        assert_eq!(r[2], Run { vcn: 0x40, len: 0x20, lcn: Some(0x140) });
        assert_eq!(r[3], Run { vcn: 0x60, len: 5, lcn: None });
    }

    #[test]
    fn attribute_list_entries() {
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[8..16].copy_from_slice(&7u64.to_le_bytes());
        e[0x10..0x18].copy_from_slice(&(42u64 | (3 << 48)).to_le_bytes());
        assert_eq!(parse_attribute_list(&e), vec![(0x80, 7, 42)]);
    }

    #[test]
    fn builds_index_with_dos_names_extensions_orphans_and_metafiles() {
        let root_ref = 5u64;
        let mut recs: Vec<Vec<u8>> = (0..24).map(|_| vec![0u8; 1024]).collect();
        recs[5] = record(0x03, 0, &[file_name_attr(5, ".", 1)]);
        // a metafile with a name must be skipped (record < 24)
        recs[11] = record(0x03, 0, &[file_name_attr(5, "$Extend", 1)]);
        // 24: dir "Док", modified 2021-01-01
        recs.push(record(0x03, 0, &[std_info(1_609_459_200, 0x10), file_name_attr(root_ref, "Док", 1)]));
        // 25: file with DOS + Win32 names, resident 10 bytes
        recs.push(record(
            0x01,
            0,
            &[
                std_info(1_700_000_000, 0x20),
                file_name_attr(24, "LONGNA~1.TXT", 2),
                file_name_attr(24, "Long Name ü.TXT", 1),
                resident(0x80, &[1u8; 10]),
            ],
        ));
        // 26: non-resident 5000 bytes, junction attribute
        recs.push(record(
            0x03,
            0,
            &[std_info(5, 0x410), file_name_attr(root_ref, "link", 1), nonresident_data(5000, 8192, 0)],
        ));
        // 27 base without attributes; 28 extension holds the name, 29 extension holds data
        recs.push(record(0x01, 0, &[]));
        recs.push(record(0x01, 27 | (1 << 48), &[file_name_attr(root_ref, "split.dat", 1)]));
        recs.push(record(0x01, 27, &[nonresident_data(777, 4096, 0)]));
        // 30: orphan (parent record 999 does not exist)
        recs.push(record(0x01, 0, &[file_name_attr(999, "lost.txt", 1), resident(0x80, &[0u8; 3])]));
        // 31: child of the metafile $Extend -> orphan, not indexed
        recs.push(record(0x01, 0, &[file_name_attr(11, "$UsnJrnl", 1)]));
        // 32: deleted (not in use)
        recs.push(record(0x00, 0, &[file_name_attr(root_ref, "deleted.txt", 1)]));

        let v = build_volume(parse_all(recs), "X:\\").unwrap();
        let paths: Vec<String> = (0..v.entries.len() as u32)
            .filter(|&i| !v.entries[i as usize].is_deleted())
            .map(|i| v.path(i))
            .collect();
        assert!(paths.contains(&"X:\\".to_string()), "{paths:?}");
        assert!(paths.contains(&"X:\\Док".to_string()));
        assert!(paths.contains(&"X:\\Док\\Long Name ü.TXT".to_string()), "{paths:?}");
        assert!(paths.contains(&"X:\\split.dat".to_string()));
        assert!(!paths.iter().any(|p| p.contains("LONGNA") || p.contains("deleted") || p.contains("lost")));
        assert!(!paths.iter().any(|p| p.contains("Extend") || p.contains("UsnJrnl")));
        assert_eq!(v.len(), 5, "root, Док, Long Name, link, split.dat: {paths:?}");

        let id = v.id_of_rec(25).unwrap();
        assert_eq!(v.entries[id as usize].size, 10);
        assert_eq!(v.entries[id as usize].mtime, 1_700_000_000);
        assert_eq!(v.lname(id), "long name ü.txt");
        let link = v.id_of_rec(26).unwrap();
        assert!(v.entries[link as usize].flags & F_REPARSE != 0);
        let split = v.id_of_rec(27).unwrap();
        assert_eq!(v.entries[split as usize].size, 777);
        assert!(v.id_of_rec(30).is_none() && v.id_of_rec(31).is_none() && v.id_of_rec(32).is_none());
    }
}
