//! File transfer engine.
//!
//! Per scenario the fastest mechanism is chosen automatically:
//!
//! | scenario                                   | mechanism                                                     |
//! |--------------------------------------------|---------------------------------------------------------------|
//! | move inside one volume                     | `MoveFileExW` rename — O(1) for any size                      |
//! | copy inside one ReFS / Dev Drive volume    | block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`)             |
//! | copy inside one volume, hard link wanted   | `CreateHardLink`                                              |
//! | many small files                           | parallel workers, one read + one write per file               |
//! | large files between volumes                | read → write pipeline, aligned unbuffered I/O, preallocation  |
//! | move between volumes                       | copy + mandatory verification + delete source                 |

pub mod aligned;
pub mod copyfile2;
pub mod hash;
pub mod meta;
pub mod pipeline;
pub mod plan;
pub mod queue;
pub mod small;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub use hash::HashAlgo;

use crate::error::{Error, IoContext, Result};
use crate::guard::Guard;
use crate::winapi;
use plan::{FileTask, Plan};

/// Pipeline block size (multiple of the 4096-byte sector alignment).
pub const BLOCK: usize = 8 << 20;
/// Files below this size go through the parallel small-file path.
pub const SMALL_MAX: u64 = 4 << 20;
/// Files at least this large get a resumable `.strela-part` + sidecar.
pub const RESUME_MIN: u64 = 64 << 20;
/// Jobs with at least this many small files measure both small-file mechanisms before choosing one.
const ADAPT_MIN_FILES: usize = 1500;
const ADAPT_BATCH: usize = 100;
const ADAPT_ROUNDS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Overwrite {
    #[default]
    Replace,
    Skip,
    /// Replace only if the source is newer.
    Newer,
}

/// Which copy mechanism to use for regular files; `Auto` picks by size, the others force one (benchmarks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Engine {
    #[default]
    Auto,
    Pipeline,
    CopyFile2,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Options {
    pub overwrite: Overwrite,
    /// Verify copies by hashing. Moves between volumes always verify (default xxh3 when `None`).
    pub verify: Option<HashAlgo>,
    /// Same-volume copy: create hard links instead of copies.
    pub hardlink: bool,
    /// Same-volume copy on ReFS / Dev Drive: clone blocks instead of copying.
    pub clone: bool,
    pub engine: Engine,
    /// Continue an interrupted job: skip identical files, continue `.strela-part` files.
    pub resume: bool,
    /// Worker threads for small files; 0 = automatic.
    pub small_threads: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            overwrite: Overwrite::Replace,
            verify: None,
            hardlink: false,
            clone: true,
            engine: Engine::Auto,
            resume: false,
            small_threads: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub op: Op,
    pub sources: Vec<PathBuf>,
    pub dest_dir: PathBuf,
    pub options: Options,
    /// Name typed by the user to confirm moving a protected folder (see [`Guard`]).
    #[serde(default)]
    pub typed_confirm: Option<String>,
}

/// Pause / cancel switch shared between the caller and the workers.
#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    interrupted: AtomicBool,
    paused: Mutex<bool>,
    cv: Condvar,
}

impl Control {
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
        self.cv.notify_all();
    }
    /// Stops like `cancel`, but partial files are kept so the job can be resumed later (application exit).
    pub fn interrupt(&self) {
        self.interrupted.store(true, Relaxed);
        self.cancel();
    }
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Relaxed)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Relaxed)
    }
    pub fn pause(&self) {
        *self.paused.lock().unwrap() = true;
    }
    pub fn resume(&self) {
        *self.paused.lock().unwrap() = false;
        self.cv.notify_all();
    }
    pub fn is_paused(&self) -> bool {
        *self.paused.lock().unwrap()
    }
    /// Blocks while paused; returns early on cancel.
    pub fn wait_if_paused(&self) {
        let mut g = self.paused.lock().unwrap();
        while *g && !self.is_cancelled() {
            g = self.cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
        }
    }
    pub fn check(&self) -> Result<()> {
        self.wait_if_paused();
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }
}

pub struct TransferProgress {
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub files_total: AtomicU64,
    pub files_done: AtomicU64,
    pub files_skipped: AtomicU64,
    pub errors: AtomicU64,
    current: Mutex<String>,
    samples: Mutex<VecDeque<(Instant, u64)>>,
    pub started: Instant,
}

impl Default for TransferProgress {
    fn default() -> Self {
        TransferProgress {
            bytes_total: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            files_skipped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            samples: Mutex::new(VecDeque::new()),
            started: Instant::now(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProgressSnapshot {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub errors: u64,
    pub current: String,
    /// Bytes per second over the last ~2 seconds.
    pub speed: f64,
    pub eta: Option<Duration>,
}

impl TransferProgress {
    pub fn add_bytes(&self, n: u64) {
        self.bytes_done.fetch_add(n, Relaxed);
    }

    pub fn set_current(&self, p: &Path) {
        if let Ok(mut c) = self.current.try_lock() {
            c.clear();
            c.push_str(&winapi::display_path(p));
        }
    }

    /// Takes a sample for the speed estimate; call from the UI at ~10 Hz.
    pub fn snapshot(&self) -> ProgressSnapshot {
        let done = self.bytes_done.load(Relaxed);
        let total = self.bytes_total.load(Relaxed);
        let now = Instant::now();
        let speed = {
            let mut s = self.samples.lock().unwrap();
            s.push_back((now, done));
            while s.len() > 2 && now.duration_since(s[0].0) > Duration::from_secs(2) {
                s.pop_front();
            }
            let (t0, b0) = s[0];
            let dt = now.duration_since(t0).as_secs_f64();
            if dt > 0.05 { done.saturating_sub(b0) as f64 / dt } else { 0.0 }
        };
        let eta = (speed > 1.0 && total > done).then(|| Duration::from_secs_f64((total - done) as f64 / speed));
        ProgressSnapshot {
            bytes_done: done,
            bytes_total: total,
            files_done: self.files_done.load(Relaxed),
            files_total: self.files_total.load(Relaxed),
            errors: self.errors.load(Relaxed),
            current: self.current.lock().map(|c| c.clone()).unwrap_or_default(),
            speed,
            eta,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ItemError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub files_copied: u64,
    pub files_skipped: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
    pub errors: Vec<ItemError>,
    /// Sources moved by renaming (no data copied).
    pub instant_moves: u64,
    /// Files made with block cloning / hard links.
    pub cloned: u64,
    pub linked: u64,
    /// Human-readable notes about the mechanisms used.
    pub notes: Vec<String>,
}

/// Everything a worker needs about the running job.
pub struct Ctx<'a> {
    pub opts: &'a Options,
    pub ctrl: &'a Control,
    pub prog: &'a TransferProgress,
    pub verify: Option<HashAlgo>,
    pub same_volume: bool,
    pub clone_ok: bool,
    pub cluster: u64,
    pub is_move: bool,
    pub cloned: AtomicU64,
    pub linked: AtomicU64,
    pub skipped: AtomicU64,
    /// Mechanism for small files in `Engine::Auto`: 0 = own worker, 1 = `CopyFile2` (see [`run`], which measures).
    pub small_engine: AtomicU8,
}

pub enum Outcome {
    Copied,
    Skipped,
}

fn identical(src: &std::fs::Metadata, dst: &std::fs::Metadata) -> bool {
    if src.len() != dst.len() {
        return false;
    }
    match (src.modified(), dst.modified()) {
        (Ok(a), Ok(b)) => {
            let d = a.duration_since(b).or_else(|e| Ok::<_, ()>(e.duration())).unwrap_or_default();
            d <= Duration::from_secs(2)
        }
        _ => false,
    }
}

/// Copies one file with the best mechanism. Returns whether anything was written.
fn exec_file(task: &FileTask, ctx: &Ctx) -> Result<Outcome> {
    ctx.ctrl.check()?;
    ctx.prog.set_current(&task.src);
    let opts = ctx.opts;
    if let Ok(dm) = std::fs::symlink_metadata(winapi::long_path(&task.dst)) {
        if dm.is_dir() {
            return Err(Error::At {
                path: task.dst.clone(),
                source: std::io::Error::other("на месте файла уже есть папка"),
            });
        }
        let sm = std::fs::metadata(winapi::long_path(&task.src)).at(&task.src)?;
        let skip = match opts.overwrite {
            Overwrite::Skip => true,
            Overwrite::Newer => matches!((sm.modified(), dm.modified()), (Ok(s), Ok(d)) if s <= d),
            Overwrite::Replace => false,
        } || (opts.resume && identical(&sm, &dm) && !pipeline::part_path(&task.dst).exists());
        if skip {
            ctx.skipped.fetch_add(1, Relaxed);
            ctx.prog.add_bytes(task.size);
            return Ok(Outcome::Skipped);
        }
    }

    if ctx.same_volume && opts.hardlink && !ctx.is_move {
        let _ = std::fs::remove_file(&task.dst);
        match std::fs::hard_link(winapi::long_path(&task.src), winapi::long_path(&task.dst)) {
            Ok(()) => {
                ctx.linked.fetch_add(1, Relaxed);
                ctx.prog.add_bytes(task.size);
                return Ok(Outcome::Copied);
            }
            Err(_) => { /* FAT/exFAT or ReFS limits: fall through to a real copy */ }
        }
    }
    if ctx.clone_ok && task.size > 0 {
        match small::clone_copy(task, ctx) {
            Ok(()) => {
                ctx.cloned.fetch_add(1, Relaxed);
                ctx.prog.add_bytes(task.size);
                return Ok(Outcome::Copied);
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(_) => { /* not cloneable (different cluster/stream layout): copy normally */ }
        }
    }

    match opts.engine {
        Engine::CopyFile2 => copyfile2::copy(task, ctx)?,
        Engine::Pipeline => pipeline::copy_large(task, ctx)?,
        Engine::Auto => {
            if task.size < SMALL_MAX {
                if ctx.small_engine.load(Relaxed) == 1 {
                    copyfile2::copy(task, ctx)?
                } else {
                    small::copy_small(task, ctx)?
                }
            } else {
                pipeline::copy_large(task, ctx)?
            }
        }
    }
    Ok(Outcome::Copied)
}

fn instant_move(src: &Path, dest_dir: &Path, opts: &Options, report: &mut Report, prog: &TransferProgress) -> Result<()> {
    let name = src.file_name().ok_or_else(|| Error::Unsupported(format!("нет имени у {}", src.display())))?;
    let dst = dest_dir.join(name);
    prog.set_current(src);
    move_in_volume(src, &dst, opts, report)?;
    prog.files_done.fetch_add(1, Relaxed);
    Ok(())
}

/// Renames `src` to `dst` on one volume; merges directories that already exist at the destination.
fn move_in_volume(src: &Path, dst: &Path, opts: &Options, report: &mut Report) -> Result<()> {
    let smeta = std::fs::symlink_metadata(winapi::long_path(src)).at(src)?;
    match std::fs::symlink_metadata(winapi::long_path(dst)) {
        Err(_) => {
            winapi::move_file_ex(src, dst, false).at(src)?;
            report.instant_moves += 1;
            Ok(())
        }
        Ok(dmeta) => {
            if smeta.is_dir() && dmeta.is_dir() {
                let entries: Vec<_> = std::fs::read_dir(winapi::long_path(src)).at(src)?.collect::<std::io::Result<_>>().at(src)?;
                for e in entries {
                    move_in_volume(&e.path(), &dst.join(e.file_name()), opts, report)?;
                }
                let _ = std::fs::remove_dir(winapi::long_path(src));
                return Ok(());
            }
            if smeta.is_dir() != dmeta.is_dir() {
                return Err(Error::At { path: dst.to_path_buf(), source: std::io::Error::other("тип назначения не совпадает") });
            }
            let skip = match opts.overwrite {
                Overwrite::Skip => true,
                Overwrite::Newer => matches!((smeta.modified(), dmeta.modified()), (Ok(s), Ok(d)) if s <= d),
                Overwrite::Replace => false,
            };
            if skip {
                report.files_skipped += 1;
                return Ok(());
            }
            if dmeta.permissions().readonly() {
                let _ = winapi::set_attributes(dst, 0);
            }
            winapi::move_file_ex(src, dst, true).at(src)?;
            report.instant_moves += 1;
            Ok(())
        }
    }
}

fn is_inside(child: &Path, parent: &Path) -> bool {
    let c = crate::guard::normalize(child);
    let p = crate::guard::normalize(parent);
    c == p || (c.len() > p.len() && c.starts_with(&p) && c.as_bytes()[p.len()] == b'\\')
}

/// Runs a transfer job to completion (or until cancelled). Per-file failures are collected in the report and do
/// not stop the job.
pub fn run(spec: &JobSpec, guard: &Guard, ctrl: &Control, prog: &TransferProgress) -> Result<Report> {
    let t0 = Instant::now();
    let mut report = Report::default();
    let dest_dir = spec.dest_dir.clone();
    std::fs::create_dir_all(winapi::long_path(&dest_dir)).at(&dest_dir)?;

    for s in &spec.sources {
        if !s.exists() && std::fs::symlink_metadata(s).is_err() {
            return Err(Error::At { path: s.clone(), source: std::io::Error::from(std::io::ErrorKind::NotFound) });
        }
        if spec.op == Op::Move {
            guard.ensure_deletable(s, spec.typed_confirm.as_deref())?;
        }
        if s.is_dir() && is_inside(&dest_dir, s) {
            return Err(Error::Unsupported(format!(
                "нельзя поместить папку «{}» саму в себя (или в её подпапку)",
                winapi::display_path(s)
            )));
        }
    }

    // Same-volume moves are plain renames.
    let mut copy_sources = Vec::new();
    for s in &spec.sources {
        if spec.op == Op::Move && winapi::same_volume(s, &dest_dir) {
            match instant_move(s, &dest_dir, &spec.options, &mut report, prog) {
                Ok(()) => {}
                Err(Error::Cancelled) => {
                    report.cancelled = true;
                    break;
                }
                Err(e) => {
                    prog.errors.fetch_add(1, Relaxed);
                    report.errors.push(ItemError { path: s.clone(), message: e.to_string() });
                }
            }
        } else {
            copy_sources.push(s.clone());
        }
    }
    if report.instant_moves > 0 {
        report.notes.push(format!("{} объектов перемещено переименованием (данные не копировались)", report.instant_moves));
    }
    if copy_sources.is_empty() || report.cancelled {
        report.elapsed = t0.elapsed();
        return Ok(report);
    }

    let plan = plan::build(&copy_sources, &dest_dir, spec.op, ctrl, prog)?;
    let Plan { dirs, files, skipped, bytes } = plan;
    for sk in skipped {
        report.errors.push(sk);
    }
    prog.bytes_total.store(bytes, Relaxed);
    prog.files_total.store(files.len() as u64, Relaxed);

    let same_volume = copy_sources.iter().all(|s| winapi::same_volume(s, &dest_dir));
    let vinfo = winapi::volume_info(&dest_dir).ok();
    let clone_ok = same_volume
        && spec.options.clone
        && spec.op == Op::Copy
        && vinfo.as_ref().is_some_and(|v| v.supports_block_cloning());
    if let Some((_, free)) = winapi::disk_space(&dest_dir)
        && !same_volume
        && bytes > free
    {
        return Err(Error::Unsupported(format!(
            "недостаточно места на диске назначения: нужно {} МБ, свободно {} МБ",
            bytes >> 20,
            free >> 20
        )));
    }
    let verify = match spec.op {
        Op::Move => Some(spec.options.verify.unwrap_or(HashAlgo::Xxh3)),
        Op::Copy => spec.options.verify,
    };
    let ctx = Ctx {
        opts: &spec.options,
        ctrl,
        prog,
        verify,
        same_volume,
        clone_ok,
        cluster: winapi::cluster_size(&dest_dir).unwrap_or(4096),
        is_move: spec.op == Op::Move,
        cloned: AtomicU64::new(0),
        linked: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
        small_engine: AtomicU8::new(0),
    };

    for d in &dirs {
        std::fs::create_dir_all(winapi::long_path(&d.dst)).at(&d.dst)?;
    }

    let errors: Mutex<Vec<ItemError>> = Mutex::new(Vec::new());
    let copied = AtomicU64::new(0);
    let process = |task: &FileTask| {
        match exec_file(task, &ctx) {
            Ok(outcome) => {
                prog.files_done.fetch_add(1, Relaxed);
                if matches!(outcome, Outcome::Copied) {
                    copied.fetch_add(1, Relaxed);
                }
                if spec.op == Op::Move && matches!(outcome, Outcome::Copied) {
                    // The copy was verified inside exec_file; only now may the source go.
                    match winapi::delete_posix(&task.src) {
                        Ok(()) => {}
                        Err(e) => errors.lock().unwrap().push(ItemError {
                            path: task.src.clone(),
                            message: format!("скопировано, но источник не удалён: {e}"),
                        }),
                    }
                }
            }
            Err(Error::Cancelled) => {}
            Err(e) => {
                prog.errors.fetch_add(1, Relaxed);
                errors.lock().unwrap().push(ItemError { path: task.src.clone(), message: e.to_string() });
            }
        }
    };

    let (small, large): (Vec<&FileTask>, Vec<&FileTask>) = match spec.options.engine {
        Engine::Pipeline => (Vec::new(), files.iter().collect()),
        _ => files.iter().partition(|t| t.size < SMALL_MAX),
    };
    if !small.is_empty() {
        let threads = if spec.options.small_threads > 0 {
            spec.options.small_threads
        } else {
            (std::thread::available_parallelism().map_or(4, |n| n.get()) * 2).clamp(4, 24)
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|e| Error::Unsupported(e.to_string()))?;
        let run_batch = |batch: &[&FileTask]| {
            pool.install(|| {
                batch.par_iter().for_each(|t| {
                    if !ctrl.is_cancelled() {
                        process(t)
                    }
                })
            })
        };
        let mut rest: &[&FileTask] = &small;
        if spec.options.engine == Engine::Auto && small.len() >= ADAPT_MIN_FILES {
            // Creating files is dominated by the file system and by antivirus filters, and which mechanism copes
            // better differs between machines (and even between folders). Measure instead of guessing: alternate
            // short batches of both mechanisms, then continue with the faster one.
            let mut spent = [Duration::ZERO; 2];
            let mut count = [0usize; 2];
            for round in 0..ADAPT_ROUNDS * 2 {
                let which = round % 2;
                let n = ADAPT_BATCH.min(rest.len());
                if n == 0 || ctrl.is_cancelled() {
                    break;
                }
                let (head, tail) = rest.split_at(n);
                ctx.small_engine.store(which as u8, Relaxed);
                let t = Instant::now();
                run_batch(head);
                spent[which] += t.elapsed();
                count[which] += n;
                rest = tail;
            }
            if count[0] > 0 && count[1] > 0 {
                let rate = |i: usize| count[i] as f64 / spent[i].as_secs_f64().max(1e-6);
                let (own, cf2) = (rate(0), rate(1));
                let pick = usize::from(cf2 > own);
                ctx.small_engine.store(pick as u8, Relaxed);
                report.notes.push(format!(
                    "мелкие файлы: замер на {} файлах — собственный путь {:.0} ф/с, CopyFile2 {:.0} ф/с → выбран {}",
                    count[0] + count[1],
                    own,
                    cf2,
                    if pick == 1 { "CopyFile2" } else { "собственный путь" }
                ));
            }
        }
        run_batch(rest);
    }
    for t in large {
        if ctrl.is_cancelled() {
            break;
        }
        process(t);
    }

    report.cancelled = ctrl.is_cancelled();
    report.errors.extend(errors.into_inner().unwrap());
    report.files_copied = copied.load(Relaxed);
    report.files_skipped = ctx.skipped.load(Relaxed);
    report.cloned = ctx.cloned.load(Relaxed);
    report.linked = ctx.linked.load(Relaxed);
    report.bytes = prog.bytes_done.load(Relaxed);

    if !report.cancelled {
        // Directory dates/attributes last (creating files changed the directories' own timestamps).
        for d in dirs.iter().rev() {
            let _ = meta::copy_dir_meta(&d.src, &d.dst);
        }
        if spec.op == Op::Move {
            // Source folders go only when every file below them was moved; remove_dir refuses non-empty ones.
            for d in dirs.iter().rev() {
                let _ = std::fs::remove_dir(winapi::long_path(&d.src));
            }
        }
    }
    report.elapsed = t0.elapsed();
    Ok(report)
}

#[cfg(test)]
mod tests;
