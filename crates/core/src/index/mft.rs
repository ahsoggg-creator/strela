//! NTFS backend: reads `$MFT` straight from the raw volume (read-only) in large sequential chunks, parses the
//! records on all cores and builds a [`VolumeIndex`]. Requires administrator rights (the indexer service).

use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{channel, sync_channel};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::ntfs::{self, ATTR_ATTRIBUTE_LIST, ATTR_BITMAP, ATTR_DATA, ChunkOut, Run};
use super::{UsnPosition, VolumeIndex};
use crate::error::{Error, Result};
use crate::progress::Progress;
use crate::winapi;

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_INVALID_FUNCTION: i32 = 1;
const CHUNK: u64 = 8 * 1024 * 1024;
const TASK_BYTES: usize = 512 * 1024;
const POOL_BUFFERS: usize = 4;

struct Layout {
    bytes_per_cluster: u64,
    record_size: usize,
    mft_start_lcn: u64,
}

pub struct ScanStats {
    pub records_total: u64,
    pub records_read: u64,
    pub read_mb_s: f64,
    pub timings: Vec<(&'static str, Duration)>,
}

/// Opens `\\.\X:` read-only and checks that it is NTFS.
fn probe(drive: char) -> Result<(File, Layout)> {
    let file = winapi::open_volume(drive).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
            Error::Unsupported(format!("нет прав администратора для чтения тома {drive}:"))
        } else {
            Error::Io(e)
        }
    })?;
    let v = winapi::ntfs_volume_data(&file).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_INVALID_FUNCTION) {
            Error::Unsupported(format!("том {drive}: не NTFS"))
        } else {
            Error::Io(e)
        }
    })?;
    let (bpc, rs, lcn) = (v.BytesPerCluster as u64, v.BytesPerFileRecordSegment as usize, v.MftStartLcn);
    if bpc == 0 || rs < 512 || !rs.is_power_of_two() || lcn <= 0 {
        return Err(Error::Ntfs("некорректные параметры тома".into()));
    }
    Ok((file, Layout { bytes_per_cluster: bpc, record_size: rs, mft_start_lcn: lcn as u64 }))
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.seek_read(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Reads `buf.len()` bytes of the `$MFT` data stream starting at virtual offset `voff`.
fn read_virtual(file: &File, runs: &[Run], bpc: u64, mut voff: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let vcn = voff / bpc;
        let run = runs
            .iter()
            .find(|r| vcn >= r.vcn && vcn < r.vcn + r.len)
            .ok_or_else(|| io::Error::other("смещение вне data runs $MFT"))?;
        let into = voff - run.vcn * bpc;
        let n = ((run.len * bpc - into) as usize).min(buf.len() - done);
        let dst = &mut buf[done..done + n];
        match run.lcn {
            Some(l) => read_exact_at(file, dst, l * bpc + into)?,
            None => dst.fill(0),
        }
        done += n;
        voff += n as u64;
    }
    Ok(())
}

fn data_extents(attr: &[u8]) -> Result<(Vec<Run>, u64)> {
    if attr[8] == 0 || attr.len() < 0x40 {
        return Err(Error::Ntfs("$DATA файла $MFT резидентный".into()));
    }
    let start = u64::from_le_bytes(attr[0x10..0x18].try_into().unwrap());
    let real = u64::from_le_bytes(attr[0x30..0x38].try_into().unwrap());
    let roff = u16::from_le_bytes([attr[0x20], attr[0x21]]) as usize;
    let mut runs = ntfs::parse_runs(attr.get(roff..).unwrap_or(&[]))?;
    for r in &mut runs {
        r.vcn += start;
    }
    Ok((runs, real))
}

type MftLayout = (Vec<Run>, u64, Option<Vec<u8>>);

fn mft_extents(file: &File, info: &Layout) -> Result<MftLayout> {
    let bpc = info.bytes_per_cluster;
    let rs = info.record_size;
    let mut block = vec![0u8; rs.max(4096)];
    read_exact_at(file, &mut block, info.mft_start_lcn * bpc)?;
    if !ntfs::apply_fixups(&mut block[..rs]) {
        return Err(Error::Ntfs("запись 0 ($MFT) повреждена".into()));
    }
    let rec0 = &block[..rs];
    let first = ntfs::find_attr(rec0, ATTR_DATA).ok_or_else(|| Error::Ntfs("в $MFT нет атрибута $DATA".into()))?;
    let (mut runs, real_size) = data_extents(first)?;

    // A heavily fragmented $MFT spills its $DATA extents into extension records listed in $ATTRIBUTE_LIST.
    if let Some(list) = ntfs::find_attr(rec0, ATTR_ATTRIBUTE_LIST) {
        if list[8] != 0 {
            return Err(Error::Unsupported("нерезидентный $ATTRIBUTE_LIST у $MFT не поддерживается".into()));
        }
        let coff = u16::from_le_bytes([list[0x14], list[0x15]]) as usize;
        let clen = u32::from_le_bytes(list[0x10..0x14].try_into().unwrap()) as usize;
        let content = list.get(coff..coff + clen).ok_or_else(|| Error::Ntfs("битый $ATTRIBUTE_LIST".into()))?;
        let mut seen = vec![0u64];
        for (ty, _vcn, rec_no) in ntfs::parse_attribute_list(content) {
            if ty != ATTR_DATA || seen.contains(&rec_no) {
                continue;
            }
            seen.push(rec_no);
            runs.sort_by_key(|r| r.vcn);
            let mut ext = vec![0u8; rs];
            read_virtual(file, &runs, bpc, rec_no * rs as u64, &mut ext)?;
            if !ntfs::apply_fixups(&mut ext) {
                return Err(Error::Ntfs(format!("расширяющая запись {rec_no} $MFT повреждена")));
            }
            if let Some(a) = ntfs::find_attr(&ext, ATTR_DATA) {
                runs.extend(data_extents(a)?.0);
            }
        }
    }
    runs.sort_by_key(|r| r.vcn);
    let bitmap = read_bitmap(file, rec0, bpc);
    Ok((runs, real_size, bitmap))
}

/// `$MFT`'s own `$BITMAP`: one bit per record, set when the record is in use.
fn read_bitmap(file: &File, rec0: &[u8], bpc: u64) -> Option<Vec<u8>> {
    const MAX_BITMAP: u64 = 256 * 1024 * 1024;
    let attr = ntfs::find_attr(rec0, ATTR_BITMAP)?;
    if attr.len() < 0x18 {
        return None;
    }
    if attr[8] == 0 {
        let clen = u32::from_le_bytes(attr[0x10..0x14].try_into().unwrap()) as usize;
        let coff = u16::from_le_bytes([attr[0x14], attr[0x15]]) as usize;
        return attr.get(coff..coff.checked_add(clen)?).map(<[u8]>::to_vec);
    }
    let (runs, real) = data_extents(attr).ok()?;
    if real == 0 || real > MAX_BITMAP {
        return None;
    }
    let mut buf = vec![0u8; real.next_multiple_of(bpc) as usize];
    read_virtual(file, &runs, bpc, 0, &mut buf).ok()?;
    buf.truncate(real as usize);
    Some(buf)
}

/// `(first record, count)` ranges worth reading. Free stretches of at least `gap` records are skipped.
fn used_ranges(bitmap: Option<&[u8]>, total: u64, gap: u64) -> Vec<(u64, u64)> {
    let Some(bm) = bitmap else {
        return vec![(0, total)];
    };
    // Records past the end of the bitmap are treated as in use (read them rather than lose files).
    let used = |i: u64| bm.get((i / 8) as usize).is_none_or(|b| b >> (i % 8) & 1 != 0);
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut i = 0u64;
    while i < total {
        if !used(i) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        i += 1;
        while i < total && i - end < gap {
            if used(i) {
                end = i + 1;
            }
            i += 1;
        }
        out.push((start, end - start));
    }
    out
}

fn skip_gap_bytes(drive: char) -> u64 {
    match winapi::incurs_seek_penalty(Path::new(&format!("{drive}:\\"))) {
        Some(true) => 1024 * 1024,
        Some(false) => 128 * 1024,
        None => 256 * 1024,
    }
}

/// Scans a whole NTFS volume. The USN journal position is taken *before* reading so that changes made during the
/// scan are replayed afterwards (applying a record twice is harmless).
pub fn scan(drive: char, progress: &Progress) -> Result<(VolumeIndex, ScanStats)> {
    let t0 = Instant::now();
    let (file, info) = probe(drive)?;
    let journal = winapi::query_or_create_usn_journal(&file).ok();
    let (runs, real_size, bitmap) = mft_extents(&file, &info)?;
    let (bpc, rs) = (info.bytes_per_cluster, info.record_size);
    let total_records = real_size / rs as u64;
    let ranges = used_ranges(bitmap.as_deref(), total_records, skip_gap_bytes(drive) / rs as u64);
    let to_read: u64 = ranges.iter().map(|r| r.1).sum();
    progress.total.store(to_read, Relaxed);
    let t_extents = t0.elapsed();

    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let note = |msg: String| {
        progress.errors.fetch_add(1, Relaxed);
        let mut v = errors.lock().unwrap();
        if v.len() < 100 {
            v.push(msg);
        }
    };

    let t1 = Instant::now();
    let read_ns = std::sync::atomic::AtomicU64::new(0);
    let per_task = (TASK_BYTES / rs).max(1);
    let per_chunk = (CHUNK / rs as u64).max(1);
    // Chunk buffers circulate: the parser hands each one back to the reader once it is done with it.
    let (tx, rx) = sync_channel::<(u64, usize, Vec<u8>)>(POOL_BUFFERS);
    let (pool_tx, pool_rx) = channel::<Vec<u8>>();
    for _ in 0..POOL_BUFFERS {
        let _ = pool_tx.send(vec![0u8; (per_chunk as usize) * rs]);
    }
    let mut outs: Vec<ChunkOut> = Vec::new();
    std::thread::scope(|s| {
        let (file, runs, ranges, note, read_ns) = (&file, &runs, &ranges, &note, &read_ns);
        s.spawn(move || {
            'ranges: for &(start, count) in ranges {
                let (mut pos, end) = (start, start + count);
                while pos < end {
                    if progress.is_cancelled() {
                        break 'ranges;
                    }
                    let Ok(mut buf) = pool_rx.recv() else { break 'ranges };
                    let n = per_chunk.min(end - pos);
                    let slice = &mut buf[..n as usize * rs];
                    let t = Instant::now();
                    if let Err(e) = read_virtual(file, runs, bpc, pos * rs as u64, slice) {
                        note(format!("чтение $MFT, запись {pos}: {e}"));
                        slice.fill(0);
                    }
                    read_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                    if tx.send((pos, n as usize, buf)).is_err() {
                        break 'ranges;
                    }
                    pos += n;
                }
            }
            drop(tx);
        });
        for (first, n, mut buf) in rx {
            if progress.is_cancelled() {
                break;
            }
            let parsed: Vec<ChunkOut> = buf[..n * rs]
                .par_chunks_mut(per_task * rs)
                .enumerate()
                .map(|(i, part)| {
                    let mut out = ChunkOut::default();
                    let base = first + (i * per_task) as u64;
                    for (j, rec) in part.chunks_exact_mut(rs).enumerate() {
                        ntfs::parse_record(rec, (base + j as u64) as u32, &mut out);
                    }
                    out
                })
                .collect();
            let named = parsed.iter().map(|c| c.bases.len() as u64).sum::<u64>();
            progress.items.fetch_add(named, Relaxed);
            progress.done.fetch_add(n as u64, Relaxed);
            outs.extend(parsed);
            let _ = pool_tx.send(buf);
        }
        drop(pool_tx);
    });
    if progress.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let t_pipeline = t1.elapsed();
    let t2 = Instant::now();
    let mut vol = ntfs::build_volume(outs, &format!("{drive}:\\"))?;
    vol.usn = journal.map(|j| UsnPosition { journal_id: j.id, next_usn: j.next_usn });
    let read_s = read_ns.load(Relaxed) as f64 / 1e9;
    let mb = (to_read * rs as u64) as f64 / 1e6;
    let stats = ScanStats {
        records_total: total_records,
        records_read: to_read,
        read_mb_s: if read_s > 0.0 { mb / read_s } else { 0.0 },
        timings: vec![
            ("экстенты $MFT и битовая карта", t_extents),
            ("чтение и разбор (конвейер)", t_pipeline),
            ("построение индекса", t2.elapsed()),
        ],
    };
    Ok((vol, stats))
}

#[cfg(test)]
mod tests {
    use super::used_ranges;

    #[test]
    fn no_bitmap_reads_everything() {
        assert_eq!(used_ranges(None, 100, 8), vec![(0, 100)]);
    }

    #[test]
    fn skips_long_gaps_and_bridges_short_ones() {
        let mut bm = vec![0u8; 4];
        for i in [0u32, 1, 4, 21] {
            bm[(i / 8) as usize] |= 1 << (i % 8);
        }
        assert_eq!(used_ranges(Some(&bm), 24, 8), vec![(0, 5), (21, 1)]);
    }

    #[test]
    fn records_past_the_bitmap_are_read() {
        let bm = [0b1u8];
        assert_eq!(used_ranges(Some(&bm), 20, 4), vec![(0, 1), (8, 12)]);
    }
}
