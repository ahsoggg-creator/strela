//! State shared between the GUI thread, the tray/hotkey thread, the IPC threads and the transfer queue.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use strela_core::guard::Guard;
use strela_core::transfer::queue::TransferQueue;
use strela_core::transfer::{JobSpec, Op, Options};

use crate::backend::Backend;
use crate::settings::Settings;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Search,
    Transfer,
    Cleanup,
    Content,
    Programs,
    Startup,
    Processes,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 8] = [Tab::Search, Tab::Transfer, Tab::Cleanup, Tab::Content, Tab::Programs, Tab::Startup, Tab::Processes, Tab::Settings];

    /// The tabs of the administrator window: it only hosts the system tools.
    pub const ADMIN: [Tab; 3] = [Tab::Programs, Tab::Startup, Tab::Processes];

    pub fn visible(admin: bool) -> &'static [Tab] {
        if admin { &Tab::ADMIN } else { &Tab::ALL }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Search => "Поиск",
            Tab::Transfer => "Передача",
            Tab::Cleanup => "Очистка",
            Tab::Content => "Содержимое",
            Tab::Programs => "Программы",
            Tab::Startup => "Автозагрузка",
            Tab::Processes => "Процессы",
            Tab::Settings => "Настройки",
        }
    }
}

/// Something the GUI has to show; produced by the context menu, the tray and the hotkey.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Show(Tab),
    Spotlight,
    PickDestination { paths: Vec<PathBuf>, op: Op },
    ConfirmDelete { paths: Vec<PathBuf>, recycle: bool },
    FindHere(PathBuf),
    FindContentHere(PathBuf),
    LargestHere(PathBuf),
}

#[derive(Default)]
pub struct Staging {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

pub struct Shared {
    pub settings: RwLock<Settings>,
    pub queue: TransferQueue,
    pub backend: Arc<Backend>,
    pub staging: Mutex<Staging>,
    events: Mutex<VecDeque<UiEvent>>,
    pub balloons: Mutex<VecDeque<(String, String, bool)>>,
    ctx: OnceLock<eframe::egui::Context>,
    pub main_hwnd: AtomicIsize,
    pub shell_hwnd: AtomicIsize,
    pub hotkey_error: Mutex<Option<String>>,
    pub quitting: AtomicBool,
    /// The elevated window of the system tools: no tray, no hotkey, no indexer, no job queue of its own.
    pub admin: bool,
    pub update: Mutex<crate::update::UpdateState>,
}

impl Shared {
    pub fn new(settings: Settings) -> Arc<Shared> {
        Shared::build(settings, false)
    }

    pub fn new_admin(settings: Settings) -> Arc<Shared> {
        Shared::build(settings, true)
    }

    fn build(settings: Settings, admin: bool) -> Arc<Shared> {
        let backend = if admin { Backend::idle() } else { Backend::new(&settings) };
        let queue = TransferQueue::new((!admin).then(|| strela_core::util::data_dir().join("queue.json")), Guard::system());
        Arc::new(Shared {
            settings: RwLock::new(settings),
            queue,
            backend,
            staging: Mutex::new(Staging::default()),
            events: Mutex::new(VecDeque::new()),
            balloons: Mutex::new(VecDeque::new()),
            ctx: OnceLock::new(),
            main_hwnd: AtomicIsize::new(0),
            shell_hwnd: AtomicIsize::new(0),
            hotkey_error: Mutex::new(None),
            quitting: AtomicBool::new(false),
            admin,
            update: Mutex::new(Default::default()),
        })
    }

    pub fn set_ctx(&self, ctx: eframe::egui::Context) {
        let _ = self.ctx.set(ctx);
    }

    pub fn repaint(&self) {
        if let Some(c) = self.ctx.get() {
            c.request_repaint();
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn push_event(&self, ev: UiEvent) {
        self.events.lock().unwrap().push_back(ev);
        self.repaint();
    }

    pub fn take_events(&self) -> Vec<UiEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }

    /// Tray balloon ("toast") — works while the window is hidden.
    pub fn notify(&self, title: &str, text: &str, error: bool) {
        self.balloons.lock().unwrap().push_back((title.into(), text.into(), error));
        crate::shell::wake(self);
    }

    pub fn show_window(&self) {
        crate::shell::show_main(self);
    }

    pub fn options_from_settings(&self) -> Options {
        let s = self.settings.read().unwrap();
        Options { verify: s.verify_copies.then_some(s.hash), ..Options::default() }
    }

    /// Queues a transfer of `paths` into `dest`.
    pub fn start_transfer(&self, paths: Vec<PathBuf>, dest: PathBuf, op: Op) {
        let n = paths.len();
        let spec = JobSpec { op, sources: paths, dest_dir: dest.clone(), options: self.options_from_settings(), typed_confirm: None };
        self.queue.add(spec);
        let verb = if op == Op::Move { "Перемещение" } else { "Копирование" };
        self.notify("Strela", &format!("{verb}: {n} элем. → {}", strela_core::winapi::display_path(&dest)), false);
        self.repaint();
    }

    /// "Paste via Strela": starts the staged copy/move into `dest`.
    pub fn paste_into(&self, dest: PathBuf) {
        let staged = {
            let mut st = self.staging.lock().unwrap();
            if st.paths.is_empty() {
                None
            } else {
                let paths = std::mem::take(&mut st.paths);
                Some((paths, st.cut))
            }
        };
        match staged {
            None => self.notify("Strela", "Буфер Strela пуст: сначала выберите «Копировать через Strela» или «Вырезать через Strela».", true),
            Some((paths, cut)) => self.start_transfer(paths, dest, if cut { Op::Move } else { Op::Copy }),
        }
    }

    /// Orderly exit: stop transfers (keeping partial files), save the index and queue, remove the tray icon.
    pub fn quit(&self) {
        if self.quitting.swap(true, Relaxed) {
            return;
        }
        self.queue.shutdown();
        self.backend.stop();
        crate::shell::remove_tray(self);
        std::process::exit(0);
    }
}
