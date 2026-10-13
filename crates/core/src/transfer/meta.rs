//! Timestamps and attributes: copied after the data so the destination looks like the source.

use std::fs::{File, FileTimes};
use std::io;
use std::os::windows::fs::{FileTimesExt, MetadataExt};
use std::path::Path;

use crate::winapi;

/// Attributes worth carrying over (read-only, hidden, system, archive, temporary, not-content-indexed).
/// Compression, encryption, sparse and reparse flags describe storage, not content, and are left alone.
pub const KEEP_ATTRS: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x2000;

pub struct SrcMeta {
    pub size: u64,
    pub attrs: u32,
    pub times: FileTimes,
}

pub fn read(meta: &std::fs::Metadata) -> SrcMeta {
    let mut times = FileTimes::new();
    if let Ok(t) = meta.accessed() {
        times = times.set_accessed(t);
    }
    if let Ok(t) = meta.modified() {
        times = times.set_modified(t);
    }
    if let Ok(t) = meta.created() {
        times = times.set_created(t);
    }
    SrcMeta { size: meta.len(), attrs: meta.file_attributes(), times }
}

/// Sets times through the still-open destination handle.
pub fn apply_times(dst: &File, m: &SrcMeta) -> io::Result<()> {
    dst.set_times(m.times)
}

pub fn apply_attrs(dst: &Path, attrs: u32) {
    let keep = attrs & KEEP_ATTRS;
    // 0x20 (archive) is the default for fresh files: skip the syscall in the common case.
    if keep != 0x20 && keep != 0 {
        let _ = winapi::set_attributes(dst, keep);
    }
}

/// Directory timestamps and attributes (opened with backup semantics, since directories are not plain files).
pub fn copy_dir_meta(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let sm = std::fs::metadata(winapi::long_path(src))?;
    let m = read(&sm);
    let d = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(winapi::SHARE_ALL)
        .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
        .open(winapi::long_path(dst))?;
    apply_times(&d, &m)?;
    drop(d);
    apply_attrs(dst, m.attrs);
    Ok(())
}
