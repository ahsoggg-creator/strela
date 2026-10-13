//! Fast deletion of big files and folders.
//!
//! * **Permanent**: a parallel tree walk — every directory lists its entries with one call, deletes the files in
//!   parallel and then itself. Each delete is a single `FileDispositionInfoEx` with POSIX semantics: the name
//!   disappears immediately, even when another process still has the file open, and read-only files need no
//!   attribute juggling. Symlinks and junctions are removed as links; their targets are never followed.
//! * **Recycle bin**: `IFileOperation` (the shell's own engine), so Explorer's "Restore" works.
//!
//! Nothing is deleted without a [`Preview`] being confirmed first — it carries the file count and total size that
//! the caller must show — and protected locations additionally need the folder's name typed in.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::guard::{Guard, Protected};
use crate::progress::Progress;
use crate::winapi::{self, RawEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Permanent,
    Recycle,
}

/// What a deletion would remove — shown to the user before anything happens.
#[derive(Clone, Debug)]
pub struct Preview {
    pub paths: Vec<PathBuf>,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Symlinks / junctions among the items (removed as links only).
    pub links: u64,
    /// Protected items and what has to be typed to unlock each.
    pub protected: Vec<(PathBuf, Protected)>,
}

/// Proof that the user saw the totals (and typed the names of protected folders). Only this can be deleted.
pub struct Confirmed {
    paths: Vec<PathBuf>,
}

impl Preview {
    /// `typed` is the text the user entered for protected items (one name unlocks all items requiring it).
    pub fn confirm(self, typed: Option<&str>) -> Result<Confirmed> {
        for (p, prot) in &self.protected {
            match typed {
                Some(t) if t.trim().eq_ignore_ascii_case(&prot.required_name) => {}
                _ => {
                    return Err(Error::Protected(format!(
                        "{} — {}; для подтверждения нужно ввести «{}»",
                        winapi::display_path(p),
                        prot.reason,
                        prot.required_name
                    )));
                }
            }
        }
        Ok(Confirmed { paths: self.paths })
    }

    pub fn needs_typed_confirmation(&self) -> Option<&Protected> {
        self.protected.first().map(|(_, p)| p)
    }
}

fn entry_name(e: &RawEntry) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(e.name)
}

/// Counts what is below `path` (parallel). Used for the confirmation text and progress totals.
fn count(path: &Path, acc: &Count, progress: &Progress) {
    if progress.is_cancelled() {
        return;
    }
    let mut kids: Vec<(PathBuf, bool, bool, u64)> = Vec::new();
    let _ = winapi::list_dir(path, |e| {
        kids.push((path.join(entry_name(&e)), e.is_dir(), e.is_reparse(), e.size));
    });
    kids.par_iter().for_each(|(p, is_dir, reparse, size)| {
        if *reparse {
            acc.links.fetch_add(1, Relaxed);
        } else if *is_dir {
            acc.dirs.fetch_add(1, Relaxed);
            count(p, acc, progress);
        } else {
            acc.files.fetch_add(1, Relaxed);
            acc.bytes.fetch_add(*size, Relaxed);
        }
    });
}

#[derive(Default)]
struct Count {
    files: std::sync::atomic::AtomicU64,
    dirs: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
    links: std::sync::atomic::AtomicU64,
}

fn big_pool() -> Result<rayon::ThreadPool> {
    // Deep folder trees recurse; give the workers room.
    rayon::ThreadPoolBuilder::new()
        .num_threads((std::thread::available_parallelism().map_or(4, |n| n.get()) * 2).clamp(4, 32))
        .stack_size(32 << 20)
        .build()
        .map_err(|e| Error::Unsupported(e.to_string()))
}

pub fn preview(paths: &[PathBuf], guard: &Guard, progress: &Progress) -> Result<Preview> {
    let acc = Count::default();
    let mut protected = Vec::new();
    let pool = big_pool()?;
    for p in paths {
        let meta = std::fs::symlink_metadata(winapi::long_path(p))
            .map_err(|e| Error::At { path: p.clone(), source: e })?;
        if let Some(prot) = guard.check(p) {
            protected.push((p.clone(), prot));
        }
        use std::os::windows::fs::MetadataExt;
        let reparse = meta.file_attributes() & winapi::FILE_ATTRIBUTE_REPARSE_POINT != 0;
        if reparse {
            acc.links.fetch_add(1, Relaxed);
        } else if meta.is_dir() {
            acc.dirs.fetch_add(1, Relaxed);
            pool.install(|| count(p, &acc, progress));
        } else {
            acc.files.fetch_add(1, Relaxed);
            acc.bytes.fetch_add(meta.len(), Relaxed);
        }
    }
    if progress.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(Preview {
        paths: paths.to_vec(),
        files: acc.files.load(Relaxed),
        dirs: acc.dirs.load(Relaxed),
        bytes: acc.bytes.load(Relaxed),
        links: acc.links.load(Relaxed),
        protected,
    })
}

#[derive(Clone, Debug, Default)]
pub struct DeleteReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    pub failed: Vec<(PathBuf, String)>,
}

struct DelCtx<'a> {
    progress: &'a Progress,
    files: std::sync::atomic::AtomicU64,
    dirs: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
    failed: Mutex<Vec<(PathBuf, String)>>,
}

impl DelCtx<'_> {
    fn fail(&self, p: &Path, e: std::io::Error) {
        self.progress.errors.fetch_add(1, Relaxed);
        let mut f = self.failed.lock().unwrap();
        if f.len() < 1000 {
            f.push((p.to_path_buf(), e.to_string()));
        }
    }
}

/// POSIX delete with a fallback for systems/file systems that lack the Ex disposition flags.
fn remove_entry(p: &Path) -> std::io::Result<()> {
    match winapi::delete_posix(p) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.raw_os_error(), Some(1 | 50 | 87)) => {
            let lp = winapi::long_path(p);
            let _ = winapi::set_attributes(p, 0);
            match std::fs::symlink_metadata(&lp) {
                Ok(m) if m.is_dir() => std::fs::remove_dir(&lp),
                _ => std::fs::remove_file(&lp),
            }
        }
        Err(e) => Err(e),
    }
}

fn delete_dir(path: &Path, ctx: &DelCtx) -> bool {
    let mut kids: Vec<(PathBuf, bool, bool, u64)> = Vec::new();
    if let Err(e) = winapi::list_dir(path, |e| {
        kids.push((path.join(entry_name(&e)), e.is_dir(), e.is_reparse(), e.size));
    }) {
        ctx.fail(path, e);
        return false;
    }
    let all_ok = kids
        .par_iter()
        .map(|(p, is_dir, reparse, size)| {
            if ctx.progress.is_cancelled() {
                return false;
            }
            if *is_dir && !*reparse {
                delete_dir(p, ctx)
            } else {
                match remove_entry(p) {
                    Ok(()) => {
                        if !*is_dir {
                            ctx.files.fetch_add(1, Relaxed);
                            ctx.bytes.fetch_add(*size, Relaxed);
                        }
                        ctx.progress.done.fetch_add(1, Relaxed);
                        true
                    }
                    Err(e) => {
                        ctx.fail(p, e);
                        false
                    }
                }
            }
        })
        .reduce(|| true, |a, b| a && b);
    if !all_ok {
        return false; // the folder still has content; it would only add a confusing second error
    }
    match remove_entry(path) {
        Ok(()) => {
            ctx.dirs.fetch_add(1, Relaxed);
            ctx.progress.done.fetch_add(1, Relaxed);
            true
        }
        Err(e) => {
            ctx.fail(path, e);
            false
        }
    }
}

/// Permanently deletes the confirmed items (parallel, POSIX semantics).
pub fn delete_permanently(c: &Confirmed, progress: &Progress) -> Result<DeleteReport> {
    let t0 = Instant::now();
    let ctx = DelCtx {
        progress,
        files: Default::default(),
        dirs: Default::default(),
        bytes: Default::default(),
        failed: Mutex::new(Vec::new()),
    };
    let pool = big_pool()?;
    pool.install(|| {
        c.paths.par_iter().for_each(|p| {
            let meta = match std::fs::symlink_metadata(winapi::long_path(p)) {
                Ok(m) => m,
                Err(e) => return ctx.fail(p, e),
            };
            use std::os::windows::fs::MetadataExt;
            let reparse = meta.file_attributes() & winapi::FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if meta.is_dir() && !reparse {
                delete_dir(p, &ctx);
            } else {
                match remove_entry(p) {
                    Ok(()) => {
                        ctx.files.fetch_add(1, Relaxed);
                        ctx.bytes.fetch_add(meta.len(), Relaxed);
                    }
                    Err(e) => ctx.fail(p, e),
                }
            }
        });
    });
    Ok(DeleteReport {
        files: ctx.files.load(Relaxed),
        dirs: ctx.dirs.load(Relaxed),
        bytes: ctx.bytes.load(Relaxed),
        elapsed: t0.elapsed(),
        failed: ctx.failed.into_inner().unwrap(),
    })
}

/// Moves the confirmed items to the recycle bin with the shell's `IFileOperation`.
pub fn delete_to_recycle_bin(c: &Confirmed) -> Result<DeleteReport> {
    let t0 = Instant::now();
    let paths = c.paths.clone();
    let handle = std::thread::Builder::new()
        .name("strela-recycle".into())
        .spawn(move || recycle::recycle(&paths))
        .map_err(Error::Io)?;
    let r = handle.join().map_err(|_| Error::Unsupported("поток корзины аварийно завершился".into()))?;
    r?;
    let mut rep = DeleteReport { elapsed: t0.elapsed(), ..Default::default() };
    for p in &c.paths {
        if std::fs::symlink_metadata(p).is_ok() {
            rep.failed.push((p.clone(), "осталось на месте (не удалось переместить в корзину)".into()));
        } else {
            rep.files += 1;
        }
    }
    Ok(rep)
}

pub mod recycle {
    use std::path::PathBuf;

    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
    use windows::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NOERRORUI, FOF_SILENT, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
        IFileOperation, IShellItem, SHCreateItemFromParsingName,
    };
    use windows::core::HSTRING;

    use crate::error::{Error, Result};

    fn err(what: &str, e: windows::core::Error) -> Error {
        Error::Unsupported(format!("{what}: {e}"))
    }

    /// Must run on a thread that may enter an STA (a fresh thread is fine).
    pub fn recycle(paths: &[PathBuf]) -> Result<()> {
        // SAFETY: balanced with CoUninitialize below; this thread owns its apartment.
        let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let result = (|| -> Result<()> {
            // SAFETY: standard COM activation of the shell's file-operation object.
            let op: IFileOperation =
                unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(|e| err("IFileOperation", e))?;
            // Windows' own safety dialogs stay on: an item that cannot be recycled (too big, network drive) must
            // not silently become a permanent delete.
            let flags = FOF_ALLOWUNDO | FOF_NOERRORUI | FOF_SILENT | FOFX_EARLYFAILURE | FOFX_RECYCLEONDELETE;
            // SAFETY: plain COM calls on a live interface.
            unsafe { op.SetOperationFlags(flags) }.map_err(|e| err("SetOperationFlags", e))?;
            for p in paths {
                let text = HSTRING::from(crate::winapi::display_path(p));
                // SAFETY: `text` outlives the call.
                let item: IShellItem = unsafe { SHCreateItemFromParsingName(&text, None) }
                    .map_err(|e| err(&format!("{}", p.display()), e))?;
                // SAFETY: valid item and no sink.
                unsafe { op.DeleteItem(&item, None) }.map_err(|e| err("DeleteItem", e))?;
            }
            // SAFETY: executes the queued operations synchronously.
            unsafe { op.PerformOperations() }.map_err(|e| err("PerformOperations", e))?;
            // SAFETY: simple getter.
            if unsafe { op.GetAnyOperationsAborted() }.map_err(|e| err("GetAnyOperationsAborted", e))?.as_bool() {
                return Err(Error::Unsupported("операция с корзиной прервана".into()));
            }
            Ok(())
        })();
        if init.is_ok() {
            // SAFETY: matches the successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn fixture(root: &Path) -> (u64, u64) {
        let mut files = 0;
        let mut bytes = 0;
        for d in 0..10 {
            let dir = root.join(format!("d{d}/sub/deeper"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..30 {
                let data = vec![1u8; 100 + f];
                fs::write(dir.join(format!("f{f}.txt")), &data).unwrap();
                fs::write(dir.parent().unwrap().join(format!("g{f}.bin")), &data).unwrap();
                files += 2;
                bytes += 2 * data.len() as u64;
            }
        }
        (files, bytes)
    }

    #[test]
    fn preview_counts_then_delete_removes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("victim");
        let (files, bytes) = fixture(&root);
        let g = Guard::system();
        let prog = Progress::default();
        let pv = preview(std::slice::from_ref(&root), &g, &prog).unwrap();
        assert_eq!((pv.files, pv.bytes), (files, bytes));
        assert_eq!(pv.dirs, 1 + 10 * 3);
        assert!(pv.protected.is_empty());
        let c = pv.confirm(None).unwrap();
        let rep = delete_permanently(&c, &Progress::default()).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!((rep.files, rep.bytes), (files, bytes));
        assert!(!root.exists());
    }

    #[test]
    fn readonly_and_open_files_are_deleted() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        fs::create_dir_all(&root).unwrap();
        let ro = root.join("ro.txt");
        fs::write(&ro, b"x").unwrap();
        winapi::set_attributes(&ro, 0x1 | 0x2 | 0x4).unwrap(); // read-only, hidden, system
        let open = root.join("open.txt");
        fs::write(&open, b"y").unwrap();
        // held open with delete sharing, as most well-behaved programs do
        let _h = fs::OpenOptions::new().read(true).share_mode(winapi::SHARE_ALL).open(&open).unwrap();
        let pv = preview(std::slice::from_ref(&root), &Guard::system(), &Progress::default()).unwrap();
        let rep = delete_permanently(&pv.confirm(None).unwrap(), &Progress::default()).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(!root.exists(), "folder gone although a file was still open (POSIX semantics)");
    }

    #[test]
    fn junctions_are_unlinked_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let precious = tmp.path().join("precious");
        fs::create_dir_all(&precious).unwrap();
        fs::write(precious.join("keep.txt"), b"must survive").unwrap();
        let victim = tmp.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(victim.join("link"))
            .arg(&precious)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        let pv = preview(std::slice::from_ref(&victim), &Guard::system(), &Progress::default()).unwrap();
        assert_eq!((pv.files, pv.links), (0, 1), "the target's files are not counted");
        let rep = delete_permanently(&pv.confirm(None).unwrap(), &Progress::default()).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(!victim.exists());
        assert_eq!(fs::read(precious.join("keep.txt")).unwrap(), b"must survive");
    }

    #[test]
    fn protected_locations_need_the_typed_name_and_are_never_touched_in_tests() {
        // Only previews are built here — nothing is deleted. A fake guard marks a temp folder as protected.
        let tmp = tempfile::tempdir().unwrap();
        let guarded = tmp.path().join("ImportantStuff");
        fs::create_dir_all(&guarded).unwrap();
        fs::write(guarded.join("a.txt"), b"a").unwrap();
        let g = Guard::from_parts(vec![(crate::guard::normalize(&guarded), "тестовая защита")], vec![]);
        let pv = preview(std::slice::from_ref(&guarded), &g, &Progress::default()).unwrap();
        assert!(pv.needs_typed_confirmation().is_some());
        assert!(pv.clone().confirm(None).is_err());
        assert!(pv.clone().confirm(Some("wrong")).is_err());
        assert!(pv.confirm(Some("importantstuff")).is_ok());
        assert!(guarded.join("a.txt").exists());

        // real system locations are flagged by the real guard (preview of C:\Windows would be slow, so check the guard)
        let sys = Guard::system();
        assert!(sys.check(Path::new(&std::env::var("SystemRoot").unwrap())).is_some());
        assert!(sys.check(Path::new("C:\\")).is_some());
        assert!(sys.check(Path::new(&std::env::var("USERPROFILE").unwrap())).is_some());
        assert!(sys.check(Path::new(&std::env::var("ProgramData").unwrap())).is_some());
        assert!(sys.check(Path::new(&std::env::var("ProgramFiles").unwrap())).is_some());
    }

    #[test]
    fn deleting_a_protected_folder_without_the_name_fails_before_any_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let guarded = tmp.path().join("Guarded");
        fs::create_dir_all(&guarded).unwrap();
        fs::write(guarded.join("a.txt"), b"a").unwrap();
        let g = Guard::from_parts(vec![(crate::guard::normalize(&guarded), "тест")], vec![]);
        let pv = preview(std::slice::from_ref(&guarded), &g, &Progress::default()).unwrap();
        assert!(pv.confirm(None).is_err());
        assert!(guarded.join("a.txt").exists());
    }

    #[test]
    #[ignore = "puts a small temp file into the real recycle bin; run with --ignored"]
    fn recycle_bin_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("strela-recycle-test.txt");
        fs::write(&f, b"bye").unwrap();
        let pv = preview(std::slice::from_ref(&f), &Guard::system(), &Progress::default()).unwrap();
        let rep = delete_to_recycle_bin(&pv.confirm(None).unwrap()).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(!f.exists());
    }
}
