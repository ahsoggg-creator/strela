//! Large files: a reader thread and a writer thread connected by a small pool of big page-aligned buffers.
//!
//! * `FILE_FLAG_NO_BUFFERING` on both sides — no double copy through the system cache, no cache pollution;
//! * the destination's space is reserved up front (`FileAllocationInfo`), so the file is not fragmented and a full
//!   disk fails immediately instead of at 99 %;
//! * the source hash (if verification is on) is computed by the reader while it streams, the destination is
//!   re-read unbuffered afterwards, so the check is against what is really on the disk;
//! * big files are written as `name.strela-part` with a sidecar recording the committed offset, so an
//!   interrupted copy continues instead of starting over.

use std::fs::OpenOptions;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::aligned::{ALIGN, AlignedBuf};
use super::hash::{Digest, Hasher, hash_file};
use super::meta;
use super::plan::FileTask;
use super::small::create_dst_with;
use super::{BLOCK, Ctx, RESUME_MIN};
use crate::error::{Error, IoContext, Result};
use crate::winapi::{self, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, SHARE_ALL};

/// Pipeline knobs. The defaults come from measurements (see docs/BENCHMARKS.md); `STRELA_TUNE` overrides them for
/// experiments: `STRELA_TUNE=src=u,dst=b,block=8192,nbuf=4` (u = unbuffered, b = buffered; block in KiB).
#[derive(Clone, Copy, Debug)]
pub struct Tune {
    pub src_unbuf: bool,
    pub dst_unbuf: bool,
    pub block: usize,
    pub nbuf: usize,
}

pub fn tune() -> &'static Tune {
    static T: std::sync::OnceLock<Tune> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = Tune { src_unbuf: true, dst_unbuf: true, block: BLOCK, nbuf: 4 };
        if let Ok(v) = std::env::var("STRELA_TUNE") {
            for kv in v.split(',') {
                match kv.split_once('=') {
                    Some(("src", x)) => t.src_unbuf = x == "u",
                    Some(("dst", x)) => t.dst_unbuf = x == "u",
                    Some(("block", x)) => t.block = x.parse::<usize>().map_or(BLOCK, |k| (k * 1024).next_multiple_of(ALIGN)),
                    Some(("nbuf", x)) => t.nbuf = x.parse().unwrap_or(4).clamp(2, 64),
                    _ => {}
                }
            }
        }
        t
    })
}
/// Flush + persist the resume sidecar after this many bytes (a static so tests can shrink it).
pub static SYNC_EVERY_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(256 << 20);
const ERROR_INVALID_PARAMETER: i32 = 87;
const ERROR_NOT_SUPPORTED: i32 = 50;

pub fn part_path(dst: &Path) -> PathBuf {
    let mut n = dst.file_name().unwrap_or_default().to_os_string();
    n.push(".strela-part");
    dst.with_file_name(n)
}

pub fn sidecar_path(dst: &Path) -> PathBuf {
    let mut n = dst.file_name().unwrap_or_default().to_os_string();
    n.push(".strela-resume");
    dst.with_file_name(n)
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Sidecar {
    v: u32,
    src: String,
    size: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
    committed: u64,
}

fn mtime_parts(m: &std::fs::Metadata) -> (u64, u32) {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos()))
}

fn write_sidecar(dst: &Path, sc: &Sidecar) {
    let path = sidecar_path(dst);
    let tmp = path.with_extension("tmp");
    if let Ok(json) = serde_json::to_vec(sc)
        && std::fs::write(winapi::long_path(&tmp), json).is_ok()
    {
        let _ = std::fs::rename(winapi::long_path(&tmp), winapi::long_path(&path));
    }
}

/// Offset to continue from, if a valid partial copy of this very source exists.
fn resume_offset(task: &FileTask, size: u64, smeta: &std::fs::Metadata) -> u64 {
    let Ok(raw) = std::fs::read(winapi::long_path(&sidecar_path(&task.dst))) else { return 0 };
    let Ok(sc) = serde_json::from_slice::<Sidecar>(&raw) else { return 0 };
    let (secs, nanos) = mtime_parts(smeta);
    let part_len = std::fs::metadata(winapi::long_path(&part_path(&task.dst))).map_or(0, |m| m.len());
    let same = sc.v == 1
        && sc.size == size
        && (sc.mtime_secs, sc.mtime_nanos) == (secs, nanos)
        && sc.src.eq_ignore_ascii_case(&task.src.to_string_lossy())
        && part_len >= sc.committed;
    if same { sc.committed / ALIGN as u64 * ALIGN as u64 } else { 0 }
}

struct Block {
    off: u64,
    len: usize,
    buf: AlignedBuf,
}

pub fn copy_large(task: &FileTask, ctx: &Ctx) -> Result<()> {
    match copy_large_inner(task, ctx, true) {
        Err(Error::At { source, .. }) | Err(Error::Io(source))
            if matches!(source.raw_os_error(), Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)) =>
        {
            // The file system refuses unbuffered I/O (some network / virtual drives): do it buffered.
            copy_large_inner(task, ctx, false)
        }
        r => r,
    }
}

fn copy_large_inner(task: &FileTask, ctx: &Ctx, unbuffered: bool) -> Result<()> {
    let t = *tune();
    let src_flags = if unbuffered && t.src_unbuf { FILE_FLAG_NO_BUFFERING } else { 0 };
    let dst_flags = if unbuffered && t.dst_unbuf { FILE_FLAG_NO_BUFFERING } else { 0 };
    let src = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(src_flags | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(winapi::long_path(&task.src))
        .at(&task.src)?;
    let smeta = src.metadata().at(&task.src)?;
    let size = smeta.len();
    let sm = meta::read(&smeta);
    let use_part = size >= RESUME_MIN;
    let target = if use_part { part_path(&task.dst) } else { task.dst.clone() };
    let start = if use_part && ctx.opts.resume { resume_offset(task, size, &smeta) } else { 0 };

    let dst = create_dst_with(&target, false, dst_flags, start == 0).at(&target)?;
    if start > 0 {
        dst.set_len(start).at(&target)?;
        ctx.prog.add_bytes(start);
    } else {
        // Failure here is harmless (FAT, network shares): the copy simply allocates as it goes.
        let _ = winapi::preallocate(&dst, size.next_multiple_of(ALIGN as u64));
    }
    let want_hash = ctx.verify.filter(|_| start == 0);

    let (data_tx, data_rx) = crossbeam_channel::bounded::<Block>(t.nbuf);
    let (free_tx, free_rx) = crossbeam_channel::bounded::<AlignedBuf>(t.nbuf);
    for _ in 0..t.nbuf {
        let _ = free_tx.send(AlignedBuf::new(t.block));
    }

    let mut inline_digest: Option<Digest> = None;
    let mut committed = start;
    let mut since_sync = 0u64;
    let outcome: Result<()> = std::thread::scope(|s| {
        let reader = s.spawn(|| -> Result<Option<Digest>> {
            let mut hasher = want_hash.map(Hasher::new);
            let mut off = start;
            while off < size {
                let Ok(mut buf) = free_rx.recv() else { break };
                let want = (size - off).min(t.block as u64) as usize;
                let padded = want.next_multiple_of(ALIGN);
                let mut got = 0usize;
                while got < want {
                    if !got.is_multiple_of(ALIGN) {
                        return Err(Error::Verify(format!("{}: короткое чтение не по границе сектора", task.src.display())));
                    }
                    let n = src.seek_read(&mut buf[got..padded], off + got as u64).at(&task.src)?;
                    if n == 0 {
                        return Err(Error::Verify(format!("{}: файл изменился во время копирования", task.src.display())));
                    }
                    got += n;
                }
                if let Some(h) = hasher.as_mut() {
                    h.update(&buf[..want]);
                }
                buf[want..padded].fill(0);
                if data_tx.send(Block { off, len: want, buf }).is_err() {
                    break;
                }
                off += want as u64;
            }
            drop(data_tx);
            Ok(hasher.map(Hasher::finish))
        });

        let mut write_loop = || -> Result<()> {
            for blk in data_rx.iter() {
                ctx.ctrl.check()?;
                let padded = blk.len.next_multiple_of(ALIGN);
                let mut done = 0;
                while done < padded {
                    let n = dst.seek_write(&blk.buf[done..padded], blk.off + done as u64).at(&target)?;
                    done += n;
                }
                committed = blk.off + blk.len as u64;
                since_sync += blk.len as u64;
                ctx.prog.add_bytes(blk.len as u64);
                let _ = free_tx.send(blk.buf);
                if use_part && since_sync >= SYNC_EVERY_BYTES.load(Relaxed) && committed < size {
                    since_sync = 0;
                    dst.sync_all().at(&target)?;
                    let (secs, nanos) = mtime_parts(&smeta);
                    write_sidecar(
                        &task.dst,
                        &Sidecar { v: 1, src: task.src.to_string_lossy().into_owned(), size, mtime_secs: secs, mtime_nanos: nanos, committed },
                    );
                }
            }
            Ok(())
        };
        let w = write_loop();
        // Unblock the reader whatever happened, then collect its result.
        drop(data_rx);
        drop(free_tx);
        let r = reader.join().unwrap_or_else(|_| Err(Error::Verify("поток чтения аварийно завершился".into())));
        match (w, r) {
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(d)) => {
                inline_digest = d;
                Ok(())
            }
        }
    });

    let cleanup = |ctx: &Ctx| {
        // A kill/interrupt keeps the partial copy for resuming; anything else removes it.
        if !ctx.ctrl.is_interrupted() {
            let _ = std::fs::remove_file(winapi::long_path(&target));
            let _ = std::fs::remove_file(winapi::long_path(&sidecar_path(&task.dst)));
        }
    };
    if let Err(e) = outcome {
        drop(dst);
        cleanup(ctx);
        return Err(e);
    }
    if committed < size && size > 0 && start < size {
        drop(dst);
        cleanup(ctx);
        return Err(Error::Cancelled);
    }

    let finish = (|| -> std::io::Result<()> {
        dst.set_len(size)?; // drop the sector padding of the last block
        meta::apply_times(&dst, &sm)?;
        if ctx.verify.is_some() || use_part {
            dst.sync_all()?;
        }
        Ok(())
    })();
    drop(dst);
    if let Err(e) = finish {
        cleanup(ctx);
        return Err(Error::At { path: target, source: e });
    }

    if let Some(algo) = ctx.verify {
        let verified = (|| -> Result<()> {
            let want = match inline_digest {
                Some(d) => d,
                None => hash_file(&task.src, algo, ctx.ctrl, None)?,
            };
            let got = hash_file(&target, algo, ctx.ctrl, None)?;
            if want == got {
                Ok(())
            } else {
                Err(Error::Verify(format!("{}: копия не совпала с оригиналом (хеш)", task.dst.display())))
            }
        })();
        if let Err(e) = verified {
            // A failed hash means the data is wrong: never keep it for resuming.
            let _ = std::fs::remove_file(winapi::long_path(&target));
            let _ = std::fs::remove_file(winapi::long_path(&sidecar_path(&task.dst)));
            return Err(e);
        }
    }

    if use_part {
        if task.dst.exists() {
            let _ = winapi::set_attributes(&task.dst, 0);
        }
        winapi::move_file_ex(&target, &task.dst, true).at(&task.dst)?;
        let _ = std::fs::remove_file(winapi::long_path(&sidecar_path(&task.dst)));
    }
    meta::apply_attrs(&task.dst, sm.attrs);
    ctx.prog.files_skipped.fetch_add(0, Relaxed);
    Ok(())
}
