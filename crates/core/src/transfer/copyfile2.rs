//! The Windows copy engine (`CopyFile2`) as a selectable mechanism. Explorer and `copy`/`xcopy` end up here; it is
//! kept for comparison benchmarks and as a fallback, and it preserves alternate data streams and security info
//! that the own pipeline does not carry.

use std::ffi::c_void;

use windows_sys::Win32::Storage::FileSystem::{
    COPYFILE2_CALLBACK_CHUNK_FINISHED, COPYFILE2_EXTENDED_PARAMETERS, COPYFILE2_MESSAGE, COPYFILE2_MESSAGE_ACTION,
    CopyFile2,
};

use super::plan::FileTask;
use super::{Control, Ctx, TransferProgress};
use crate::error::{Error, Result};
use crate::winapi::{self, wide};

const COPY_FILE_NO_BUFFERING: u32 = 0x1000;
const PROGRESS_CONTINUE: COPYFILE2_MESSAGE_ACTION = 0;
const PROGRESS_CANCEL: COPYFILE2_MESSAGE_ACTION = 1;

struct CbCtx<'a> {
    ctrl: &'a Control,
    prog: &'a TransferProgress,
}

unsafe extern "system" fn callback(msg: *const COPYFILE2_MESSAGE, ctx: *const c_void) -> COPYFILE2_MESSAGE_ACTION {
    // SAFETY: CopyFile2 passes back the pointer given in `pvCallbackContext`, which outlives the call.
    let cb = unsafe { &*(ctx as *const CbCtx) };
    if cb.ctrl.is_cancelled() {
        return PROGRESS_CANCEL;
    }
    // SAFETY: `msg` is valid for the duration of the callback; the union member read matches `Type`.
    unsafe {
        if (*msg).Type == COPYFILE2_CALLBACK_CHUNK_FINISHED {
            cb.prog.add_bytes((*msg).Info.ChunkFinished.uliChunkSize);
        }
    }
    PROGRESS_CONTINUE
}

/// Copies one file with `CopyFile2` (unbuffered for large files, like Explorer).
pub fn copy(task: &FileTask, ctx: &Ctx) -> Result<()> {
    copy_with(&task.src, &task.dst, task.size, ctx.ctrl, ctx.prog)?;
    if let Some(algo) = ctx.verify {
        super::hash::verify_copy(&task.src, &task.dst, algo, task.size, ctx.ctrl)?;
    }
    Ok(())
}

pub fn copy_with(src: &std::path::Path, dst: &std::path::Path, size: u64, ctrl: &Control, prog: &TransferProgress) -> Result<()> {
    let s = wide(&winapi::long_path(src));
    let d = wide(&winapi::long_path(dst));
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(winapi::long_path(parent));
    }
    let cb = CbCtx { ctrl, prog };
    let mut cancel = 0i32;
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: if size >= 64 << 20 { COPY_FILE_NO_BUFFERING } else { 0 },
        pfCancel: &mut cancel,
        pProgressRoutine: Some(callback),
        pvCallbackContext: &cb as *const CbCtx as *mut c_void,
    };
    // SAFETY: both paths are NUL-terminated; `params`, `cancel` and `cb` outlive the (synchronous) call.
    let mut hr = unsafe { CopyFile2(s.as_ptr(), d.as_ptr(), &params) };
    // 0x80070005 = access denied: usually a read-only/hidden destination being replaced.
    if hr == 0x8007_0005u32 as i32 && dst.exists() {
        let _ = winapi::set_attributes(dst, 0);
        // SAFETY: as above.
        hr = unsafe { CopyFile2(s.as_ptr(), d.as_ptr(), &params) };
    }
    if hr >= 0 {
        return Ok(());
    }
    let _ = std::fs::remove_file(winapi::long_path(dst));
    if ctrl.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let code = (hr as u32 & 0xFFFF) as i32;
    Err(Error::At { path: dst.to_path_buf(), source: std::io::Error::from_raw_os_error(code) })
}
