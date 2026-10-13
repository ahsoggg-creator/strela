use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant, SystemTime};

use super::*;

fn guard() -> Guard {
    Guard::system()
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i * 131 + seed as usize) % 253) as u8).collect()
}

fn job(op: Op, sources: Vec<PathBuf>, dest: &Path, options: Options) -> JobSpec {
    JobSpec { op, sources, dest_dir: dest.to_path_buf(), options, typed_confirm: None }
}

fn exec(spec: &JobSpec) -> Report {
    run(spec, &guard(), &Control::default(), &TransferProgress::default()).unwrap()
}

/// A temp dir on a different volume than `%TEMP%`: the repository's own `target` folder. `None` when the
/// machine has just one volume (the cross-volume tests then do nothing).
fn other_volume_dir(base: &Path) -> Option<tempfile::TempDir> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("target").join("testtmp");
    fs::create_dir_all(&here).ok()?;
    if winapi::same_volume(&here, base) {
        return None;
    }
    tempfile::Builder::new().prefix("xv-").tempdir_in(here).ok()
}

fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut ents: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.push((format!("{rel}\\"), Vec::new()));
                walk(&p, base, out);
            } else {
                out.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    let mut v = Vec::new();
    walk(root, root, &mut v);
    v
}

fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::create_dir_all(root.join("пустая")).unwrap();
    fs::write(root.join("top.txt"), b"top").unwrap();
    fs::write(root.join("a/one.bin"), pattern(100_000, 1)).unwrap();
    fs::write(root.join("a/b/Файл с пробелом.dat"), pattern(5 << 20, 2)).unwrap(); // above SMALL_MAX
    fs::write(root.join("a/b/empty"), b"").unwrap();
}

#[test]
fn copies_a_tree_with_every_engine() {
    for engine in [Engine::Auto, Engine::Pipeline, Engine::CopyFile2] {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        make_tree(&src);
        let dst = tmp.path().join("dst");
        let rep = exec(&job(
            Op::Copy,
            vec![src.clone()],
            &dst,
            Options { engine, verify: Some(HashAlgo::Blake3), ..Default::default() },
        ));
        assert!(rep.errors.is_empty(), "{engine:?}: {:?}", rep.errors);
        assert_eq!(rep.files_copied, 4, "{engine:?}");
        assert_eq!(tree(&src), tree(&dst.join("src")), "{engine:?}");
        assert!(tree(&dst.join("src")).iter().any(|(n, _)| n == "пустая\\"), "empty dirs are copied");
    }
}

#[test]
fn preserves_times_and_attributes() {
    use std::os::windows::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("s.txt");
    fs::write(&src, b"hello").unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::options().write(true).open(&src).unwrap().set_modified(old).unwrap();
    winapi::set_attributes(&src, 0x1 | 0x2).unwrap(); // read-only + hidden
    let dst = tmp.path().join("out");
    for engine in [Engine::Auto, Engine::Pipeline] {
        let rep = exec(&job(Op::Copy, vec![src.clone()], &dst, Options { engine, ..Default::default() }));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        let m = fs::metadata(dst.join("s.txt")).unwrap();
        assert_eq!(m.modified().unwrap(), old, "{engine:?}");
        assert_eq!(m.file_attributes() & 0x3, 0x3, "{engine:?}: read-only and hidden kept");
        // re-copy over the read-only destination must work (Replace)
        let rep = exec(&job(Op::Copy, vec![src.clone()], &dst, Options { engine, ..Default::default() }));
        assert!(rep.errors.is_empty(), "{engine:?} replace read-only: {:?}", rep.errors);
    }
    winapi::set_attributes(&src, 0).unwrap();
}

#[test]
fn pipeline_handles_odd_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let sizes = [0usize, 1, 511, 4095, 4096, 4097, BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK + 4097];
    for (i, n) in sizes.iter().enumerate() {
        fs::write(src.join(format!("f{i}")), pattern(*n, i as u8)).unwrap();
    }
    let dst = tmp.path().join("dst");
    let rep = exec(&job(
        Op::Copy,
        vec![src.clone()],
        &dst,
        Options { engine: Engine::Pipeline, verify: Some(HashAlgo::Xxh3), ..Default::default() },
    ));
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    for (i, n) in sizes.iter().enumerate() {
        let got = fs::read(dst.join("src").join(format!("f{i}"))).unwrap();
        assert_eq!(got.len(), *n);
        assert!(got == pattern(*n, i as u8), "size {n}");
    }
}

#[test]
fn big_file_leaves_no_partials_and_reports_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big.bin");
    let data = pattern(80 << 20, 9); // above RESUME_MIN: goes through .strela-part
    fs::write(&src, &data).unwrap();
    let dst = tmp.path().join("out");
    let prog = TransferProgress::default();
    let rep = run(
        &job(Op::Copy, vec![src], &dst, Options { verify: Some(HashAlgo::Blake3), ..Default::default() }),
        &guard(),
        &Control::default(),
        &prog,
    )
    .unwrap();
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert!(fs::read(dst.join("big.bin")).unwrap() == data);
    let names: Vec<_> =
        fs::read_dir(&dst).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["big.bin"], "no .strela-part / .strela-resume left");
    let snap = prog.snapshot();
    assert_eq!(snap.bytes_done, 80 << 20);
    assert_eq!(snap.bytes_total, 80 << 20);
}

#[test]
fn same_volume_move_is_a_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("deep")).unwrap();
    // 300 MiB would take seconds to copy; a rename must take no time at all.
    let big = fs::File::create(src.join("deep/huge.bin")).unwrap();
    big.set_len(300 << 20).unwrap();
    drop(big);
    fs::write(src.join("small.txt"), b"x").unwrap();
    let dst = tmp.path().join("dst");
    let t = Instant::now();
    let rep = exec(&job(Op::Move, vec![src.clone()], &dst, Options::default()));
    let took = t.elapsed();
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert_eq!(rep.instant_moves, 1);
    assert_eq!(rep.bytes, 0, "no data was copied");
    assert!(took < Duration::from_millis(500), "move took {took:?}");
    assert!(!src.exists());
    assert_eq!(fs::metadata(dst.join("src/deep/huge.bin")).unwrap().len(), 300 << 20);
}

#[test]
fn same_volume_move_merges_into_existing_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("p"), tmp.path().join("d"));
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::create_dir_all(dst.join("p/sub")).unwrap();
    fs::write(src.join("sub/new.txt"), b"new").unwrap();
    fs::write(src.join("sub/both.txt"), b"from-src").unwrap();
    fs::write(dst.join("p/sub/both.txt"), b"from-dst").unwrap();
    fs::write(dst.join("p/sub/keep.txt"), b"keep").unwrap();
    let rep = exec(&job(Op::Move, vec![src.clone()], &dst, Options::default()));
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert_eq!(fs::read(dst.join("p/sub/both.txt")).unwrap(), b"from-src");
    assert_eq!(fs::read(dst.join("p/sub/keep.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(dst.join("p/sub/new.txt")).unwrap(), b"new");
    assert!(!src.exists());
}

#[test]
fn cross_volume_move_verifies_then_removes_source() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(other) = other_volume_dir(tmp.path()) else {
        eprintln!("skipped: only one volume available");
        return;
    };
    let src = tmp.path().join("src");
    make_tree(&src);
    let expect = tree(&src);
    let rep = exec(&job(Op::Move, vec![src.clone()], other.path(), Options::default()));
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert_eq!(rep.instant_moves, 0, "different volumes cannot be renamed");
    assert!(!src.exists(), "source removed after verified copy");
    assert_eq!(tree(&other.path().join("src")), expect);
}

#[test]
fn cross_volume_copy_of_a_big_file_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(other) = other_volume_dir(tmp.path()) else { return };
    let data = pattern(100 << 20, 5);
    let src = tmp.path().join("big.bin");
    fs::write(&src, &data).unwrap();
    for engine in [Engine::Auto, Engine::CopyFile2] {
        let rep = exec(&job(
            Op::Copy,
            vec![src.clone()],
            other.path(),
            Options { engine, verify: Some(HashAlgo::Blake3), ..Default::default() },
        ));
        assert!(rep.errors.is_empty(), "{engine:?}: {:?}", rep.errors);
        assert!(fs::read(other.path().join("big.bin")).unwrap() == data, "{engine:?}");
    }
}

#[test]
fn interrupted_big_copy_resumes_from_the_committed_offset() {
    pipeline::SYNC_EVERY_BYTES.store(16 << 20, Relaxed);
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big.bin");
    let data = pattern(300 << 20, 7);
    fs::write(&src, &data).unwrap();
    let dst = tmp.path().join("out");
    let spec = job(Op::Copy, vec![src.clone()], &dst, Options { verify: Some(HashAlgo::Blake3), ..Default::default() });

    let ctrl = Control::default();
    let prog = TransferProgress::default();
    std::thread::scope(|s| {
        s.spawn(|| {
            let t = Instant::now();
            while prog.bytes_done.load(Relaxed) < 100 << 20 && t.elapsed() < Duration::from_secs(60) {
                std::thread::sleep(Duration::from_millis(2));
            }
            ctrl.interrupt();
        });
        let rep = run(&spec, &guard(), &ctrl, &prog).unwrap();
        assert!(rep.cancelled);
    });
    let part = pipeline::part_path(&dst.join("big.bin"));
    let side = pipeline::sidecar_path(&dst.join("big.bin"));
    assert!(part.exists() && side.exists(), "partial copy and sidecar are kept on interrupt");
    assert!(!dst.join("big.bin").exists());

    let mut spec2 = spec.clone();
    spec2.options.resume = true;
    let prog2 = TransferProgress::default();
    let rep = run(&spec2, &guard(), &Control::default(), &prog2).unwrap();
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert!(fs::read(dst.join("big.bin")).unwrap() == data, "resumed copy is byte-identical");
    assert!(!part.exists() && !side.exists());
    assert_eq!(prog2.bytes_done.load(Relaxed), 300 << 20);
    pipeline::SYNC_EVERY_BYTES.store(256 << 20, Relaxed);
}

#[test]
fn cancel_removes_partial_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big.bin");
    fs::write(&src, pattern(200 << 20, 3)).unwrap();
    let dst = tmp.path().join("out");
    let ctrl = Control::default();
    let prog = TransferProgress::default();
    let spec = job(Op::Copy, vec![src], &dst, Options::default());
    std::thread::scope(|s| {
        s.spawn(|| {
            while prog.bytes_done.load(Relaxed) < 20 << 20 {
                std::thread::sleep(Duration::from_millis(1));
            }
            ctrl.cancel();
        });
        let rep = run(&spec, &guard(), &ctrl, &prog).unwrap();
        assert!(rep.cancelled);
    });
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0, "nothing left behind");
}

#[test]
fn pause_stops_progress_until_resumed() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big.bin");
    fs::write(&src, pattern(200 << 20, 4)).unwrap();
    let dst = tmp.path().join("out");
    let ctrl = Control::default();
    let prog = TransferProgress::default();
    let spec = job(Op::Copy, vec![src], &dst, Options::default());
    std::thread::scope(|s| {
        s.spawn(|| {
            while prog.bytes_done.load(Relaxed) < 8 << 20 {
                std::thread::sleep(Duration::from_millis(1));
            }
            ctrl.pause();
            std::thread::sleep(Duration::from_millis(300)); // in-flight blocks drain
            let a = prog.bytes_done.load(Relaxed);
            std::thread::sleep(Duration::from_millis(500));
            let b = prog.bytes_done.load(Relaxed);
            assert_eq!(a, b, "no progress while paused");
            ctrl.resume();
        });
        let rep = run(&spec, &guard(), &ctrl, &prog).unwrap();
        assert!(!rep.cancelled && rep.errors.is_empty());
    });
    assert_eq!(fs::metadata(dst.join("big.bin")).unwrap().len(), 200 << 20);
}

#[test]
fn overwrite_policies() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("f.txt"), tmp.path().join("d"));
    fs::create_dir_all(&dst).unwrap();
    fs::write(&src, b"NEW").unwrap();
    fs::write(dst.join("f.txt"), b"old").unwrap();
    let old = SystemTime::now() - Duration::from_secs(86_400);
    fs::File::options().write(true).open(dst.join("f.txt")).unwrap().set_modified(old).unwrap();

    let rep = exec(&job(Op::Copy, vec![src.clone()], &dst, Options { overwrite: Overwrite::Skip, ..Default::default() }));
    assert_eq!((rep.files_skipped, fs::read(dst.join("f.txt")).unwrap()), (1, b"old".to_vec()));
    let rep = exec(&job(Op::Copy, vec![src.clone()], &dst, Options { overwrite: Overwrite::Newer, ..Default::default() }));
    assert_eq!((rep.files_skipped, fs::read(dst.join("f.txt")).unwrap()), (0, b"NEW".to_vec()), "source is newer");
    let rep = exec(&job(Op::Copy, vec![src.clone()], &dst, Options { overwrite: Overwrite::Newer, ..Default::default() }));
    assert_eq!(rep.files_skipped, 1, "now identical age: not newer");
}

#[test]
fn copy_next_to_itself_gets_a_new_name() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("doc.txt");
    fs::write(&src, b"x").unwrap();
    let rep = exec(&job(Op::Copy, vec![src.clone()], tmp.path(), Options::default()));
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert_eq!(fs::read(tmp.path().join("doc (копия).txt")).unwrap(), b"x");
    assert_eq!(fs::read(&src).unwrap(), b"x");
}

#[test]
fn folder_into_itself_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a");
    fs::create_dir_all(src.join("b")).unwrap();
    let r = run(
        &job(Op::Copy, vec![src.clone()], &src.join("b"), Options::default()),
        &guard(),
        &Control::default(),
        &TransferProgress::default(),
    );
    assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
    let r = run(
        &job(Op::Move, vec![src.clone()], &src, Options::default()),
        &guard(),
        &Control::default(),
        &TransferProgress::default(),
    );
    assert!(r.is_err());
}

#[test]
fn hardlink_option_shares_the_data() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("orig.txt");
    fs::write(&src, b"shared").unwrap();
    let dst = tmp.path().join("links");
    let rep = exec(&job(Op::Copy, vec![src.clone()], &dst, Options { hardlink: true, ..Default::default() }));
    assert_eq!(rep.linked, 1, "{rep:?}");
    // a write through one name is visible through the other
    fs::write(&src, b"changed!").unwrap();
    assert_eq!(fs::read(dst.join("orig.txt")).unwrap(), b"changed!");
}

#[test]
fn long_paths_and_unicode() {
    let tmp = tempfile::tempdir().unwrap();
    let mut deep = tmp.path().join("src");
    for i in 0..14 {
        deep = deep.join(format!("очень-длинное-имя-папки-номер-{i}-xxxxxxxxxxxx"));
    }
    assert!(deep.as_os_str().len() > 300);
    fs::create_dir_all(winapi::long_path(&deep)).unwrap();
    fs::write(winapi::long_path(&deep.join("файл.txt")), b"deep").unwrap();
    let dst = tmp.path().join("dst");
    let rep = exec(&job(Op::Copy, vec![tmp.path().join("src")], &dst, Options::default()));
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert_eq!(rep.files_copied, 1);
}

#[test]
fn moving_system_folders_is_refused_before_anything_happens() {
    let tmp = tempfile::tempdir().unwrap();
    let win = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    let r = run(
        &job(Op::Move, vec![win.clone()], tmp.path(), Options::default()),
        &guard(),
        &Control::default(),
        &TransferProgress::default(),
    );
    assert!(matches!(r, Err(Error::Protected(_))), "{r:?}");
    assert!(win.join("System32").exists());
    let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    let r = run(
        &job(Op::Move, vec![profile], tmp.path(), Options::default()),
        &guard(),
        &Control::default(),
        &TransferProgress::default(),
    );
    assert!(matches!(r, Err(Error::Protected(_))));
}

#[test]
fn one_unreadable_file_does_not_stop_the_job() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("ok1.txt"), b"1").unwrap();
    fs::write(src.join("locked.txt"), b"2").unwrap();
    fs::write(src.join("ok2.txt"), b"3").unwrap();
    // exclusive lock: no sharing at all
    let _lock = fs::OpenOptions::new().read(true).share_mode(0).open(src.join("locked.txt")).unwrap();
    let dst = tmp.path().join("dst");
    let rep = exec(&job(Op::Copy, vec![src], &dst, Options::default()));
    assert_eq!(rep.errors.len(), 1, "{:?}", rep.errors);
    assert!(rep.errors[0].path.ends_with("locked.txt"));
    assert_eq!(fs::read(dst.join("src/ok1.txt")).unwrap(), b"1");
    assert_eq!(fs::read(dst.join("src/ok2.txt")).unwrap(), b"3");
}

#[test]
fn queue_runs_jobs_and_survives_restart() {
    use queue::{JobState, TransferQueue};
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.txt");
    fs::write(&src, b"queue").unwrap();
    let state = tmp.path().join("queue.json");
    let q = TransferQueue::new(Some(state.clone()), guard());
    let id = q.add(job(Op::Copy, vec![src.clone()], &tmp.path().join("out"), Options::default()));
    let t = Instant::now();
    while !q.list().iter().any(|j| j.id == id && j.state.is_finished()) && t.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(q.list()[0].state, JobState::Done);
    assert_eq!(fs::read(tmp.path().join("out/a.txt")).unwrap(), b"queue");
    q.clear_finished();

    // a running big job is interrupted by shutdown and comes back as Interrupted, then completes on resume
    fs::write(tmp.path().join("big.bin"), pattern(400 << 20, 1)).unwrap();
    let id = q.add(job(Op::Copy, vec![tmp.path().join("big.bin")], &tmp.path().join("out2"), Options::default()));
    let t = Instant::now();
    while q.list().iter().find(|j| j.id == id).is_some_and(|j| j.progress.bytes_done < 30 << 20)
        && t.elapsed() < Duration::from_secs(30)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    q.shutdown();
    let q2 = TransferQueue::new(Some(state), guard());
    let l = q2.list();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].state, JobState::Interrupted);
    q2.resume(id);
    let t = Instant::now();
    while !q2.list().iter().any(|j| j.state.is_finished()) && t.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(q2.list()[0].state, JobState::Done);
    assert_eq!(fs::metadata(tmp.path().join("out2/big.bin")).unwrap().len(), 400 << 20);
    q2.shutdown();
}
