//! Job queue: jobs run one after another on a worker thread; each can be paused, resumed or cancelled. The queue is
//! persisted, so jobs that were running when the program exited come back as "interrupted" and can be continued
//! (partial big files are picked up where they stopped).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Control, JobSpec, ProgressSnapshot, Report, TransferProgress, run};
use crate::error::Error;
use crate::guard::Guard;

pub type JobId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    /// Finished, but some files failed.
    DoneWithErrors,
    Failed,
    Cancelled,
    /// The program exited while the job was unfinished.
    Interrupted,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(self, JobState::Done | JobState::DoneWithErrors | JobState::Failed | JobState::Cancelled)
    }
}

#[derive(Clone, Debug)]
pub struct JobInfo {
    pub id: JobId,
    pub spec: JobSpec,
    pub state: JobState,
    pub progress: ProgressSnapshot,
    /// Recent speed samples (bytes/s) for the live graph.
    pub speed_history: Vec<f32>,
    pub report: Option<Report>,
    pub error: Option<String>,
}

struct Entry {
    id: JobId,
    spec: JobSpec,
    state: JobState,
    ctrl: Arc<Control>,
    prog: Arc<TransferProgress>,
    report: Option<Report>,
    error: Option<String>,
    history: VecDeque<f32>,
    last_sample: Instant,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    id: JobId,
    spec: JobSpec,
    state: JobState,
}

struct Inner {
    jobs: Mutex<Vec<Entry>>,
    cv: Condvar,
    stop: AtomicBool,
    next_id: AtomicU64,
    persist: Option<PathBuf>,
    guard: Guard,
}

pub struct TransferQueue {
    inner: Arc<Inner>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Inner {
    fn save(&self, jobs: &[Entry]) {
        let Some(path) = &self.persist else { return };
        let saved: Vec<Saved> = jobs
            .iter()
            .filter(|j| !j.state.is_finished())
            .map(|j| Saved { id: j.id, spec: j.spec.clone(), state: j.state })
            .collect();
        if let Ok(json) = serde_json::to_vec_pretty(&saved) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

impl TransferQueue {
    pub fn new(persist: Option<PathBuf>, guard: Guard) -> TransferQueue {
        let mut jobs = Vec::new();
        let mut max_id = 0;
        if let Some(p) = &persist
            && let Ok(raw) = std::fs::read(p)
            && let Ok(saved) = serde_json::from_slice::<Vec<Saved>>(&raw)
        {
            for s in saved {
                max_id = max_id.max(s.id);
                jobs.push(Entry {
                    id: s.id,
                    spec: s.spec,
                    state: JobState::Interrupted,
                    ctrl: Arc::new(Control::default()),
                    prog: Arc::new(TransferProgress::default()),
                    report: None,
                    error: None,
                    history: VecDeque::new(),
                    last_sample: Instant::now(),
                });
            }
        }
        let inner = Arc::new(Inner {
            jobs: Mutex::new(jobs),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            next_id: AtomicU64::new(max_id + 1),
            persist,
            guard,
        });
        let w = inner.clone();
        let worker = std::thread::Builder::new()
            .name("strela-transfer".into())
            .spawn(move || worker_loop(w))
            .expect("spawn transfer worker");
        TransferQueue { inner, worker: Mutex::new(Some(worker)) }
    }

    pub fn add(&self, spec: JobSpec) -> JobId {
        let id = self.inner.next_id.fetch_add(1, Relaxed);
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.push(Entry {
            id,
            spec,
            state: JobState::Queued,
            ctrl: Arc::new(Control::default()),
            prog: Arc::new(TransferProgress::default()),
            report: None,
            error: None,
            history: VecDeque::new(),
            last_sample: Instant::now(),
        });
        self.inner.save(&jobs);
        self.inner.cv.notify_all();
        id
    }

    fn with<R>(&self, id: JobId, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        let mut jobs = self.inner.jobs.lock().unwrap();
        let r = jobs.iter_mut().find(|j| j.id == id).map(f);
        self.inner.save(&jobs);
        self.inner.cv.notify_all();
        r
    }

    pub fn pause(&self, id: JobId) {
        self.with(id, |j| {
            if j.state == JobState::Running {
                j.ctrl.pause();
                j.state = JobState::Paused;
            }
        });
    }

    pub fn resume(&self, id: JobId) {
        self.with(id, |j| match j.state {
            JobState::Paused => {
                j.ctrl.resume();
                j.state = JobState::Running;
            }
            JobState::Interrupted => {
                // A fresh run that continues partial files and skips what is already identical.
                j.spec.options.resume = true;
                j.ctrl = Arc::new(Control::default());
                j.prog = Arc::new(TransferProgress::default());
                j.state = JobState::Queued;
            }
            _ => {}
        });
    }

    pub fn cancel(&self, id: JobId) {
        self.with(id, |j| match j.state {
            JobState::Queued | JobState::Interrupted => j.state = JobState::Cancelled,
            JobState::Running | JobState::Paused => {
                j.ctrl.cancel();
                j.ctrl.resume();
            }
            _ => {}
        });
    }

    pub fn remove(&self, id: JobId) {
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.retain(|j| j.id != id || !(j.state.is_finished() || j.state == JobState::Interrupted));
        self.inner.save(&jobs);
    }

    pub fn clear_finished(&self) {
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.retain(|j| !j.state.is_finished());
        self.inner.save(&jobs);
    }

    pub fn list(&self) -> Vec<JobInfo> {
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.iter_mut()
            .map(|j| {
                let progress = j.prog.snapshot();
                if matches!(j.state, JobState::Running | JobState::Paused)
                    && j.last_sample.elapsed() >= Duration::from_millis(250)
                {
                    j.last_sample = Instant::now();
                    j.history.push_back(if j.state == JobState::Paused { 0.0 } else { progress.speed as f32 });
                    while j.history.len() > 240 {
                        j.history.pop_front();
                    }
                }
                JobInfo {
                    id: j.id,
                    spec: j.spec.clone(),
                    state: j.state,
                    progress,
                    speed_history: j.history.iter().copied().collect(),
                    report: j.report.clone(),
                    error: j.error.clone(),
                }
            })
            .collect()
    }

    pub fn is_idle(&self) -> bool {
        self.inner.jobs.lock().unwrap().iter().all(|j| !matches!(j.state, JobState::Queued | JobState::Running | JobState::Paused))
    }

    /// Stops the running job without discarding partial files and saves the queue.
    pub fn shutdown(&self) {
        self.inner.stop.store(true, Relaxed);
        {
            let jobs = self.inner.jobs.lock().unwrap();
            for j in jobs.iter().filter(|j| matches!(j.state, JobState::Running | JobState::Paused)) {
                j.ctrl.interrupt();
                j.ctrl.resume();
            }
        }
        self.inner.cv.notify_all();
        if let Some(w) = self.worker.lock().unwrap().take() {
            let _ = w.join();
        }
        let mut jobs = self.inner.jobs.lock().unwrap();
        for j in jobs.iter_mut() {
            if matches!(j.state, JobState::Running | JobState::Paused | JobState::Queued) {
                j.state = JobState::Interrupted;
            }
        }
        self.inner.save(&jobs);
    }
}

fn worker_loop(inner: Arc<Inner>) {
    loop {
        let (id, spec, ctrl, prog) = {
            let mut jobs = inner.jobs.lock().unwrap();
            loop {
                if inner.stop.load(Relaxed) {
                    return;
                }
                if let Some(j) = jobs.iter_mut().find(|j| j.state == JobState::Queued) {
                    j.state = JobState::Running;
                    break (j.id, j.spec.clone(), j.ctrl.clone(), j.prog.clone());
                }
                jobs = inner.cv.wait_timeout(jobs, Duration::from_millis(500)).unwrap().0;
            }
        };
        let result = run(&spec, &inner.guard, &ctrl, &prog);
        let mut jobs = inner.jobs.lock().unwrap();
        if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
            match result {
                Ok(rep) => {
                    j.state = if ctrl.is_interrupted() {
                        JobState::Interrupted
                    } else if rep.cancelled {
                        JobState::Cancelled
                    } else if rep.errors.is_empty() {
                        JobState::Done
                    } else {
                        JobState::DoneWithErrors
                    };
                    j.report = Some(rep);
                }
                Err(Error::Cancelled) => {
                    j.state = if ctrl.is_interrupted() { JobState::Interrupted } else { JobState::Cancelled }
                }
                Err(e) => {
                    j.state = JobState::Failed;
                    j.error = Some(e.to_string());
                }
            }
        }
        inner.save(&jobs);
    }
}
