//! Where search results come from: the indexer service when it runs (MFT + USN, shared by every program), or an
//! in-process indexer (directory walk) when it does not.

use std::path::PathBuf;
use std::sync::Arc;

use std::sync::Mutex;
use strela_core::indexer::{Config, Indexer};
use strela_core::ipc::{self, Request, Response, RootDto, RowDto, SERVICE_PIPE};

use crate::settings::Settings;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Service,
    Local,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub mode: Mode,
    pub roots: Vec<RootDto>,
}

pub struct SearchOut {
    pub total: usize,
    pub elapsed_us: u64,
    pub rows: Vec<RowDto>,
}

pub struct Backend {
    local: Mutex<Option<Arc<Indexer>>>,
    mode: Mutex<Mode>,
    use_service: Mutex<bool>,
    roots: Mutex<Vec<String>>,
}

fn service_alive() -> Option<bool> {
    match ipc::call::<_, Response>(SERVICE_PIPE, &Request::Ping) {
        Ok(Response::Pong { elevated, .. }) => Some(elevated),
        _ => None,
    }
}

impl Backend {
    pub fn new(s: &Settings) -> Arc<Backend> {
        let b = Arc::new(Backend {
            local: Mutex::new(None),
            mode: Mutex::new(Mode::Local),
            use_service: Mutex::new(s.use_service),
            roots: Mutex::new(s.index_roots.clone()),
        });
        b.refresh();
        b
    }

    pub fn apply_settings(&self, s: &Settings) {
        *self.use_service.lock().unwrap() = s.use_service;
        let changed = {
            let mut r = self.roots.lock().unwrap();
            let changed = *r != s.index_roots;
            *r = s.index_roots.clone();
            changed
        };
        if changed && let Some(ix) = self.local.lock().unwrap().take() {
            ix.stop();
        }
        self.refresh();
    }

    /// Picks the service when reachable, otherwise makes sure the local indexer runs. Cheap; call every few seconds.
    pub fn refresh(&self) {
        let want_service = *self.use_service.lock().unwrap() && service_alive().is_some();
        if want_service {
            *self.mode.lock().unwrap() = Mode::Service;
            if let Some(ix) = self.local.lock().unwrap().take() {
                ix.stop(); // frees the memory of the duplicate index
            }
            return;
        }
        *self.mode.lock().unwrap() = Mode::Local;
        let mut l = self.local.lock().unwrap();
        if l.is_none() {
            let roots = self.roots.lock().unwrap().clone();
            let data = strela_core::util::data_dir().join("index.bin");
            let mut cfg = Config::all_fixed_drives(data, strela_core::winapi::is_elevated());
            if !roots.is_empty() {
                cfg.roots = roots.into_iter().map(PathBuf::from).collect();
            }
            *l = Some(Arc::new(Indexer::start(cfg)));
        }
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    pub fn request(&self, req: Request) -> Result<Response, String> {
        match self.mode() {
            Mode::Service => ipc::call(SERVICE_PIPE, &req).map_err(|e| format!("служба индексации: {e}")),
            Mode::Local => {
                let ix = self.local.lock().unwrap().clone().ok_or("индекс не запущен")?;
                Ok(ipc::handle(&ix, req))
            }
        }
    }

    pub fn search(&self, query: &str, regex: bool, fuzzy: bool, limit: usize) -> Result<SearchOut, String> {
        match self.request(Request::Search { query: query.into(), regex, fuzzy, limit })? {
            Response::Search { total, elapsed_us, rows } => Ok(SearchOut { total, elapsed_us, rows }),
            Response::Error { message } => Err(message),
            other => Err(format!("неожиданный ответ: {other:?}")),
        }
    }

    pub fn files(&self, filter: &str, limit: usize) -> Result<(Vec<PathBuf>, bool), String> {
        match self.request(Request::Files { filter: filter.into(), limit })? {
            Response::Files { paths, truncated } => Ok((paths.into_iter().map(PathBuf::from).collect(), truncated)),
            Response::Error { message } => Err(message),
            other => Err(format!("неожиданный ответ: {other:?}")),
        }
    }

    pub fn status(&self) -> Status {
        let mode = self.mode();
        let roots = match self.request(Request::Status) {
            Ok(Response::Status { roots }) => roots,
            _ => Vec::new(),
        };
        Status { mode, roots }
    }

    pub fn rescan(&self) {
        let _ = self.request(Request::Rescan);
    }

    /// Index-backed "largest" when the index covers the path.
    pub fn largest_from_index(&self, path: &str, top: usize) -> Option<strela_core::largest::Largest> {
        match self.request(Request::Largest { path: path.into(), top }).ok()? {
            Response::Largest { files, folders, total_files, total_bytes } => {
                let mk = |v: Vec<(String, u64)>, dirs: bool| {
                    v.into_iter().map(|(p, s)| strela_core::largest::Big { path: PathBuf::from(p), size: s, is_dir: dirs }).collect()
                };
                Some(strela_core::largest::Largest { files: mk(files, false), folders: mk(folders, true), total_files, total_bytes })
            }
            _ => None,
        }
    }

    pub fn stop(&self) {
        if let Some(ix) = self.local.lock().unwrap().take() {
            ix.stop();
        }
    }
}
