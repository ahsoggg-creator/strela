//! Orchestrates the index: loads the saved snapshot (instant start), scans each root with the best available
//! backend, keeps it fresh (USN journal for NTFS volumes, directory watcher for walked folders) and saves it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::index::{SharedIndex, VolumeKind, mft, shared, store, usn, walk, watch};
use crate::progress::Progress;
use crate::winapi;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Loading,
    Scanning,
    Watching,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct RootStatus {
    pub root: String,
    pub kind: VolumeKind,
    pub phase: Phase,
    pub entries: usize,
    pub progress: f32,
    pub last_scan: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Snapshot file (loaded at start, saved periodically and on stop).
    pub data_file: PathBuf,
    /// Roots to index. Drive roots (`C:\`) may use the MFT backend; other folders are always walked.
    pub roots: Vec<PathBuf>,
    /// Use `$MFT` + USN where possible (needs administrator rights / the service).
    pub use_mft: bool,
    pub save_interval: Duration,
    /// Scans run with low CPU / I/O priority (the GUI); the CLI wants full speed.
    pub background: bool,
}

impl Config {
    /// All local fixed drives.
    pub fn all_fixed_drives(data_file: PathBuf, use_mft: bool) -> Config {
        Config {
            data_file,
            roots: winapi::drive_letters(false).into_iter().map(|d| PathBuf::from(format!("{d}:\\"))).collect(),
            use_mft,
            save_interval: Duration::from_secs(300),
            background: false,
        }
    }
}

struct Shared {
    index: SharedIndex,
    cfg: Config,
    stop: AtomicBool,
    dirty: AtomicBool,
    rescan_gen: AtomicU64,
    status: Mutex<Vec<RootStatus>>,
    progress: Vec<Arc<Progress>>,
}

pub struct Indexer {
    shared: Arc<Shared>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// Diagnostics for support: `STRELA_DEBUG=1` prints indexer decisions to stderr.
fn trace(msg: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("STRELA_DEBUG").is_some()) {
        eprintln!("[indexer] {}", msg());
    }
}

fn drive_of(root: &Path) -> Option<char> {
    let s = winapi::display_path(root);
    let b = s.as_bytes();
    let is_drive_root = (b.len() == 2 || (b.len() == 3 && b[2] == b'\\')) && b[1] == b':' && b[0].is_ascii_alphabetic();
    is_drive_root.then(|| (b[0] as char).to_ascii_uppercase())
}

impl Indexer {
    pub fn index(&self) -> SharedIndex {
        self.shared.index.clone()
    }

    pub fn status(&self) -> Vec<RootStatus> {
        let mut st = self.shared.status.lock().clone();
        for (s, p) in st.iter_mut().zip(&self.shared.progress) {
            if s.phase == Phase::Scanning {
                s.progress = p.fraction();
                // walk scans have no known total: report how many entries were found so far
                if p.total.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    s.entries = p.items.load(std::sync::atomic::Ordering::Relaxed) as usize;
                }
            }
        }
        st
    }

    pub fn is_ready(&self) -> bool {
        self.status().iter().all(|s| s.phase != Phase::Loading && s.phase != Phase::Scanning)
    }

    /// Forces a fresh scan of every root.
    pub fn rescan(&self) {
        self.shared.rescan_gen.fetch_add(1, Relaxed);
    }

    pub fn start(cfg: Config) -> Indexer {
        let index = shared();
        let t0 = Instant::now();
        // Instant start: the snapshot is searchable before any scan finishes.
        let loaded = store::load(&cfg.data_file).ok();
        if let Some(mut snap) = loaded {
            let mut g = index.write();
            for v in snap.volumes.drain(..) {
                let wanted = cfg.roots.iter().any(|r| winapi::display_path(r).to_lowercase() == v.root.to_lowercase());
                if wanted {
                    g.set_volume(v);
                }
            }
        }
        let status: Vec<RootStatus> = cfg
            .roots
            .iter()
            .map(|r| {
                let root = winapi::display_path(r);
                let g = index.read();
                let (entries, kind) = g
                    .volume_for_root(&root)
                    .map(|i| (g.volumes[i].len(), g.volumes[i].kind))
                    .unwrap_or((0, VolumeKind::Walk));
                RootStatus {
                    root,
                    kind,
                    phase: Phase::Loading,
                    entries,
                    progress: 0.0,
                    last_scan: (entries > 0).then(|| t0.elapsed()),
                }
            })
            .collect();
        let progress = cfg.roots.iter().map(|_| Arc::new(Progress::default())).collect();
        let shared = Arc::new(Shared {
            index,
            cfg,
            stop: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            rescan_gen: AtomicU64::new(0),
            status: Mutex::new(status),
            progress,
        });
        let mut threads = Vec::new();
        for i in 0..shared.cfg.roots.len() {
            let sh = shared.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("strela-index-{i}"))
                    .spawn(move || run_root(sh, i))
                    .expect("spawn indexer thread"),
            );
        }
        {
            let sh = shared.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("strela-index-save".into())
                    .spawn(move || save_loop(sh))
                    .expect("spawn saver thread"),
            );
        }
        Indexer { shared, threads: Mutex::new(threads) }
    }

    /// Stops background work and writes the snapshot.
    pub fn stop(&self) {
        self.shared.stop.store(true, Relaxed);
        for p in &self.shared.progress {
            p.cancel();
        }
        // A thread stuck in a system call must not keep the program from quitting: give them a few seconds, then
        // carry on (the process exit ends whatever is left).
        let threads: Vec<_> = self.threads.lock().drain(..).collect();
        let deadline = Instant::now() + Duration::from_secs(8);
        for t in threads {
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                trace(|| "an indexer thread did not stop in time".into());
            }
        }
        save_now(&self.shared);
    }
}

fn save_now(sh: &Shared) {
    let bytes = {
        let g = sh.index.read();
        if g.is_empty() {
            return;
        }
        store::encode(&g)
    };
    let _ = store::write_atomic(&bytes, &sh.cfg.data_file);
    sh.dirty.store(false, Relaxed);
}

fn save_loop(sh: Arc<Shared>) {
    let mut last = Instant::now();
    while !sh.stop.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        if last.elapsed() >= sh.cfg.save_interval && sh.dirty.load(Relaxed) {
            save_now(&sh);
            last = Instant::now();
        }
    }
}

fn set_phase(sh: &Shared, i: usize, phase: Phase) {
    let entries = {
        let g = sh.index.read();
        g.volume_for_root(&sh.status.lock()[i].root).map_or(0, |v| g.volumes[v].len())
    };
    let mut st = sh.status.lock();
    st[i].phase = phase;
    st[i].entries = entries;
}

fn sleep_unless(sh: &Shared, ms: u64) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(ms) && !sh.stop.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_root(sh: Arc<Shared>, i: usize) {
    let root_path = sh.cfg.roots[i].clone();
    let root = winapi::display_path(&root_path);
    let prog = sh.progress[i].clone();
    let mft_drive = drive_of(&root_path).filter(|_| {
        sh.cfg.use_mft && winapi::volume_info(&root_path).is_ok_and(|v| v.is_ntfs()) && winapi::is_elevated()
    });
    let mut first = true;
    let mut last_scan = Instant::now() - Duration::from_secs(3600);
    let mut seen_gen = sh.rescan_gen.load(Relaxed);
    while !sh.stop.load(Relaxed) {
        let has_snapshot = {
            let g = sh.index.read();
            g.volume_for_root(&root).map(|v| g.volumes[v].kind)
        };
        if let Some(drive) = mft_drive {
            // 1) replay the journal on top of the snapshot when it is still valid
            if first && has_snapshot == Some(VolumeKind::Mft) {
                set_phase(&sh, i, Phase::Watching);
            } else {
                // a journal that keeps failing must not turn into a scan loop
                if last_scan.elapsed() < Duration::from_secs(60) {
                    sleep_unless(&sh, 60_000);
                    if sh.stop.load(Relaxed) {
                        break;
                    }
                }
                last_scan = Instant::now();
                trace(|| format!("{root}: MFT scan"));
                match scan_mft(&sh, i, drive, &root, &prog) {
                    Ok(()) => trace(|| format!("{root}: MFT scan done")),
                    Err(msg) => {
                        trace(|| format!("{root}: MFT scan failed: {msg}"));
                        set_phase(&sh, i, Phase::Failed(msg));
                        sleep_unless(&sh, 5000);
                        continue;
                    }
                }
            }
            first = false;
            let st = &sh.stop;
            let dirty = &sh.dirty;
            match usn::follow(drive, &root, &sh.index, st, &|| dirty.store(true, Relaxed)) {
                usn::Outcome::Stopped => break,
                // keep serving the old index until the next scan replaces it
                usn::Outcome::NeedsRescan(why) => {
                    trace(|| format!("{root}: journal says rescan: {why}"));
                    continue;
                }
            }
        } else {
            // Walk backend: serve the snapshot while a fresh scan runs, then follow with a watcher.
            let stale = has_snapshot == Some(VolumeKind::Walk);
            if stale && first {
                set_phase(&sh, i, Phase::Scanning);
            }
            first = false;
            if let Err(msg) = scan_walk(&sh, i, &root_path, &root, &prog) {
                if sh.stop.load(Relaxed) {
                    break;
                }
                set_phase(&sh, i, Phase::Failed(msg));
                sleep_unless(&sh, 10_000);
                continue;
            }
            let rescan_flag = Arc::new(AtomicBool::new(false));
            let rf = rescan_flag.clone();
            let dirty = Arc::new(AtomicBool::new(false));
            let d2 = dirty.clone();
            let watcher = watch::start(
                root_path.clone(),
                sh.index.clone(),
                Arc::new(move || d2.store(true, Relaxed)),
                Arc::new(move || rf.store(true, Relaxed)),
            );
            let watcher = match watcher {
                Ok(w) => Some(w),
                Err(e) => {
                    set_phase(&sh, i, Phase::Failed(format!("слежение за изменениями: {e}")));
                    None
                }
            };
            while !sh.stop.load(Relaxed) && !rescan_flag.load(Relaxed) && sh.rescan_gen.load(Relaxed) == seen_gen {
                if dirty.swap(false, Relaxed) {
                    sh.dirty.store(true, Relaxed);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if let Some(w) = watcher {
                w.stop();
            }
            if sh.stop.load(Relaxed) {
                break;
            }
        }
        if sh.rescan_gen.load(Relaxed) != seen_gen {
            seen_gen = sh.rescan_gen.load(Relaxed);
            sh.index.write().volumes.retain(|v| v.root.to_lowercase() != root.to_lowercase());
        }
    }
}

fn scan_mft(sh: &Shared, i: usize, drive: char, root: &str, prog: &Arc<Progress>) -> Result<(), String> {
    set_phase(sh, i, Phase::Scanning);
    let t = Instant::now();
    let (vol, _stats) = mft::scan(drive, prog).map_err(|e| e.to_string())?;
    sh.index.write().set_volume(vol);
    sh.dirty.store(true, Relaxed);
    let mut st = sh.status.lock();
    st[i].kind = VolumeKind::Mft;
    st[i].last_scan = Some(t.elapsed());
    drop(st);
    let _ = root;
    set_phase(sh, i, Phase::Watching);
    Ok(())
}

fn scan_walk(sh: &Shared, i: usize, root_path: &Path, root: &str, prog: &Arc<Progress>) -> Result<(), String> {
    set_phase(sh, i, Phase::Scanning);
    let t = Instant::now();
    prog.items.store(0, Relaxed);
    prog.total.store(0, Relaxed);
    let vol = walk::scan_root_with(root_path, prog, sh.cfg.background).map_err(|e| e.to_string())?;
    sh.index.write().set_volume(vol);
    sh.dirty.store(true, Relaxed);
    let mut st = sh.status.lock();
    st[i].kind = VolumeKind::Walk;
    st[i].last_scan = Some(t.elapsed());
    drop(st);
    let _ = root;
    set_phase(sh, i, Phase::Watching);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Options, Query};

    fn wait_ready(ix: &Indexer) {
        let t = Instant::now();
        while !ix.is_ready() && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ix.is_ready(), "{:?}", ix.status());
    }

    fn find(ix: &Indexer, q: &str) -> Vec<String> {
        let o = Options { fuzzy: false, ..Options::default() };
        let g = ix.index();
        let g = g.read();
        g.search(&Query::parse(q, &o).unwrap(), &o).hits.iter().map(|h| g.row(h).name).collect()
    }

    #[test]
    fn scans_watches_saves_and_restarts_from_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/alpha.txt"), b"1").unwrap();
        let snap = dir.path().join("index.bin");
        let cfg = Config { data_file: snap.clone(), roots: vec![root.clone()], use_mft: false, save_interval: Duration::from_secs(3600), background: false };

        let ix = Indexer::start(cfg.clone());
        wait_ready(&ix);
        assert_eq!(find(&ix, "alpha"), vec!["alpha.txt"]);
        std::fs::write(root.join("sub/beta.txt"), b"22").unwrap();
        let t = Instant::now();
        while find(&ix, "beta").is_empty() && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(find(&ix, "beta"), vec!["beta.txt"], "watcher must add the new file");
        ix.stop();
        assert!(snap.exists());

        // restart: the snapshot is searchable immediately, before any scan
        std::fs::write(root.join("sub/gamma.txt"), b"3").unwrap();
        let ix2 = Indexer::start(cfg);
        assert_eq!(find(&ix2, "beta"), vec!["beta.txt"], "served from the snapshot right away");
        wait_ready(&ix2);
        let t = Instant::now();
        while find(&ix2, "gamma").is_empty() && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(find(&ix2, "gamma"), vec!["gamma.txt"], "refresh scan picks up changes made while offline");
        ix2.stop();
    }
}
