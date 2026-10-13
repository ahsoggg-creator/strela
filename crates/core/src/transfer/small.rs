//! Small files: one read, one write, run by many workers at once. Per-file latency (open/create/close, directory
//! updates) dominates for these, so throughput comes from parallelism and from touching the file system as few
//! times as possible — no temp file + rename, no extra attribute call in the common case.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use super::hash::hash_bytes;
use super::meta;
use super::plan::FileTask;
use super::{Ctx, Error, Result};
use crate::error::IoContext;
use crate::winapi::{self, FILE_FLAG_SEQUENTIAL_SCAN, SHARE_ALL};

thread_local! {
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Creates the destination, clearing read-only/hidden attributes of an existing file if needed.
/// `extra_flags` are `FILE_FLAG_*` bits; `truncate = false` keeps existing content (resuming a partial copy).
pub fn create_dst_with(dst: &Path, read_too: bool, extra_flags: u32, truncate: bool) -> std::io::Result<File> {
    let p = winapi::long_path(dst);
    let open = || {
        OpenOptions::new()
            .write(true)
            .read(read_too)
            .create(true)
            .truncate(truncate)
            .share_mode(winapi::SHARE_ALL & !0x4)
            .custom_flags(extra_flags)
            .open(&p)
    };
    match open() {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && p.exists() => {
            winapi::set_attributes(dst, 0)?;
            open()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            open()
        }
        r => r,
    }
}

pub fn create_dst(dst: &Path, read_too: bool) -> std::io::Result<File> {
    create_dst_with(dst, read_too, 0, true)
}

pub fn copy_small(task: &FileTask, ctx: &Ctx) -> Result<()> {
    let mut src = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(winapi::long_path(&task.src))
        .at(&task.src)?;
    let smeta = src.metadata().at(&task.src)?;
    let sm = meta::read(&smeta);
    BUF.with_borrow_mut(|buf| -> Result<()> {
        buf.clear();
        buf.reserve(sm.size as usize);
        src.read_to_end(buf).at(&task.src)?;
        let digest = ctx.verify.map(|a| hash_bytes(a, buf));
        let mut out = create_dst(&task.dst, false).at(&task.dst)?;
        let res = (|| -> std::io::Result<()> {
            out.write_all(buf)?;
            meta::apply_times(&out, &sm)
        })();
        drop(out);
        if let Err(e) = res {
            let _ = std::fs::remove_file(winapi::long_path(&task.dst));
            return Err(Error::At { path: task.dst.clone(), source: e });
        }
        meta::apply_attrs(&task.dst, sm.attrs);
        if let (Some(algo), Some(want)) = (ctx.verify, digest) {
            let got = std::fs::read(winapi::long_path(&task.dst)).at(&task.dst)?;
            if hash_bytes(algo, &got) != want {
                let _ = std::fs::remove_file(winapi::long_path(&task.dst));
                return Err(Error::Verify(format!("{}: копия не совпала с оригиналом", task.dst.display())));
            }
        }
        Ok(())
    })?;
    ctx.prog.add_bytes(sm.size);
    Ok(())
}

/// Block cloning on ReFS / Dev Drive: the destination shares the source's clusters (copy-on-write).
pub fn clone_copy(task: &FileTask, ctx: &Ctx) -> Result<()> {
    let src = OpenOptions::new().read(true).share_mode(SHARE_ALL).open(winapi::long_path(&task.src)).at(&task.src)?;
    let smeta = src.metadata().at(&task.src)?;
    let sm = meta::read(&smeta);
    let dst = create_dst(&task.dst, true).at(&task.dst)?;
    let r = winapi::clone_file(&src, &dst, sm.size, ctx.cluster).and_then(|()| meta::apply_times(&dst, &sm));
    drop(dst);
    match r {
        Ok(()) => {
            meta::apply_attrs(&task.dst, sm.attrs);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(winapi::long_path(&task.dst));
            Err(Error::At { path: task.dst.clone(), source: e })
        }
    }
}
