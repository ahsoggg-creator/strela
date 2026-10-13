//! The window: tabs, the Spotlight popup, dialogs, toasts.

pub mod actions;
pub mod cleanup;
pub mod content;
pub mod dialogs;
pub mod icons;
pub mod installer;
pub mod prefs;
pub mod search;
pub mod theme;
pub mod transfer;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Frame, Id, Key, Margin, Modifiers, RichText, TextureHandle, Ui, ViewportCommand, WindowLevel, pos2, vec2};
use strela_core::transfer::queue::JobState;
use strela_core::transfer::Op;
use strela_core::util;

use crate::backend::{Mode as BackendMode, Status};
use crate::state::{Shared, Tab, UiEvent};
use cleanup::CleanupTab;
use content::ContentTab;
use dialogs::{DeleteDialog, PickDialog};
use icons::Icons;
use prefs::PrefsTab;
use search::SearchPane;
use theme::pal;
use transfer::TransferTab;

pub const LOGO: &[u8] = include_bytes!("../../assets/icon64.rgba");

pub enum Screen {
    Main,
    Spotlight,
    Install(installer::Install),
    Uninstall(installer::Uninstall),
}

struct Toast {
    text: String,
    error: bool,
    born: Instant,
}

pub struct App {
    pub shared: Arc<Shared>,
    pub screen: Screen,
    tab: Tab,
    pub icons: Icons,
    pub logo: Option<TextureHandle>,
    main_search: SearchPane,
    spot_search: SearchPane,
    transfer: TransferTab,
    cleanup: CleanupTab,
    content: ContentTab,
    prefs: PrefsTab,
    delete: Option<DeleteDialog>,
    pick: Option<PickDialog>,
    status: Arc<Mutex<Status>>,
    toasts: Vec<Toast>,
    spot_shown: Instant,
    spot_rows_applied: usize,
    return_to_main: bool,
    saved_rect: Option<egui::Rect>,
    hidden: bool,
    start_hidden: bool,
    frame_no: u64,
    index_stamp: u64,
    prev_tab: Option<Tab>,
    focus_search_on_tab: bool,
    geometry_pending: bool,
}

pub fn logo_image() -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([64, 64], LOGO)
}

impl App {
    pub fn new(cc: &eframe::CreationContext, shared: Arc<Shared>, screen: Screen, start_hidden: bool) -> App {
        shared.set_ctx(cc.egui_ctx.clone());
        theme::install_fonts(&cc.egui_ctx);
        theme::install(&cc.egui_ctx, &shared.settings());
        let logo = Some(cc.egui_ctx.load_texture("logo", logo_image(), egui::TextureOptions::LINEAR));
        let status = Arc::new(Mutex::new(shared.backend.status()));
        {
            // keeps the backend choice and the index status fresh without blocking the UI
            let (sh, st) = (shared.clone(), status.clone());
            std::thread::Builder::new()
                .name("strela-status".into())
                .spawn(move || {
                    let mut n = 0u32;
                    loop {
                        std::thread::sleep(Duration::from_millis(700));
                        n += 1;
                        if sh.quitting.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        if n.is_multiple_of(7) {
                            sh.backend.refresh();
                        }
                        let s = sh.backend.status();
                        *st.lock().unwrap() = s;
                        sh.repaint();
                    }
                })
                .expect("spawn status thread");
        }
        let in_app = matches!(screen, Screen::Main);
        App {
            main_search: SearchPane::new(shared.clone(), 2000),
            spot_search: SearchPane::new(shared.clone(), 60),
            transfer: TransferTab::new(&shared),
            cleanup: CleanupTab::new(),
            content: ContentTab::new(),
            prefs: PrefsTab::new(),
            shared,
            screen,
            tab: Tab::Search,
            icons: Icons::default(),
            logo,
            delete: None,
            pick: None,
            status,
            toasts: Vec::new(),
            spot_shown: Instant::now(),
            spot_rows_applied: usize::MAX,
            return_to_main: true,
            saved_rect: None,
            hidden: start_hidden && in_app,
            start_hidden: start_hidden && in_app,
            frame_no: 0,
            index_stamp: 0,
            prev_tab: None,
            focus_search_on_tab: true,
            geometry_pending: false,
        }
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast { text: text.into(), error, born: Instant::now() });
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        for ev in self.shared.take_events() {
            match ev {
                UiEvent::Show(tab) => {
                    if matches!(self.screen, Screen::Spotlight) {
                        self.leave_spotlight(ctx, false);
                    }
                    self.tab = tab;
                    self.hidden = false;
                }
                UiEvent::Spotlight => self.enter_spotlight(ctx),
                UiEvent::PickDestination { paths, op } => {
                    if matches!(self.screen, Screen::Spotlight) {
                        self.leave_spotlight(ctx, false);
                    }
                    self.hidden = false;
                    self.pick = Some(PickDialog::new(paths, op, &self.shared.settings().last_dest));
                }
                UiEvent::ConfirmDelete { paths, recycle } => {
                    if matches!(self.screen, Screen::Spotlight) {
                        self.leave_spotlight(ctx, false);
                    }
                    self.hidden = false;
                    self.delete = Some(DeleteDialog::new(paths, recycle, ctx));
                }
                UiEvent::FindHere(p) => {
                    if matches!(self.screen, Screen::Spotlight) {
                        self.leave_spotlight(ctx, false);
                    }
                    self.hidden = false;
                    self.tab = Tab::Search;
                    let dir = strela_core::winapi::display_path(&p);
                    self.main_search.set_query(format!("path:\"{dir}\" "));
                    self.main_search.focus = true;
                }
                UiEvent::LargestHere(p) => {
                    if matches!(self.screen, Screen::Spotlight) {
                        self.leave_spotlight(ctx, false);
                    }
                    self.hidden = false;
                    self.tab = Tab::Cleanup;
                    self.cleanup.path = strela_core::winapi::display_path(&p);
                    self.cleanup.start_now = true;
                }
            }
        }
    }

    fn enter_spotlight(&mut self, ctx: &egui::Context) {
        if !matches!(self.screen, Screen::Spotlight) {
            self.return_to_main = !self.hidden;
            self.saved_rect = ctx.input(|i| i.viewport().inner_rect);
            self.screen = Screen::Spotlight;
            ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
            self.spot_rows_applied = usize::MAX;
        }
        self.hidden = false;
        self.spot_shown = Instant::now();
        self.spot_search.set_query("");
        self.spot_search.focus = true;
        self.place_spotlight(ctx, 0);
    }

    fn place_spotlight(&mut self, ctx: &egui::Context, rows: usize) {
        if self.spot_rows_applied == rows {
            return;
        }
        self.spot_rows_applied = rows;
        let h = 96.0 + rows as f32 * 46.0 + if rows > 0 { 38.0 } else { 0.0 };
        let w = 760.0;
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(w, h)));
        let mon = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2((mon.x - w) / 2.0, mon.y * 0.16)));
    }

    fn leave_spotlight(&mut self, ctx: &egui::Context, hide: bool) {
        self.screen = Screen::Main;
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
        ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
        let size = self.saved_rect.map_or(vec2(1180.0, 760.0), |r| r.size());
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        // inner rect -> outer position: step back by the title bar/border the window gets back
        if let Some(r) = self.saved_rect {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(r.min.x - 8.0, (r.min.y - 31.0).max(0.0))));
        }
        if hide || !self.return_to_main {
            self.hide(ctx);
        }
    }

    fn hide(&mut self, _ctx: &egui::Context) {
        self.hidden = true;
        crate::shell::hide_main(&self.shared);
    }

    pub fn main_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let p = pal(ui);
        // top bar
        egui::Panel::top("top").frame(Frame::new().fill(p.panel).inner_margin(Margin::symmetric(16, 10))).show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(l) = &self.logo {
                    ui.add(egui::Image::new(l).fit_to_exact_size(vec2(30.0, 30.0)));
                }
                ui.label(RichText::new("Strela").size(19.0).strong());
                ui.add_space(14.0);
                for t in Tab::ALL {
                    if theme::tab_pill(ui, t.title(), self.tab == t).clicked() {
                        self.tab = t;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let st = self.status.lock().unwrap().clone();
                    let total: usize = st.roots.iter().map(|r| r.entries).sum();
                    let scanning = st.roots.iter().find(|r| r.phase == "scanning");
                    let (text, color) = match (st.mode, scanning) {
                        (_, Some(r)) if r.progress > 0.0 => (format!("индексация {}  {:.0}%", r.root, r.progress * 100.0), p.warn),
                        (_, Some(r)) => (format!("индексация {} · найдено {}", r.root, fmt_count(r.entries)), p.warn),
                        (BackendMode::Service, _) => (format!("служба · MFT · {} записей", fmt_count(total)), p.ok),
                        (BackendMode::Local, _) => (format!("индекс · {} записей", fmt_count(total)), p.accent),
                    };
                    theme::chip(ui, &text, color);
                    ui.label(theme::dim(ui, self.shared.settings().hotkey.describe()));
                });
            });
        });
        // bottom bar
        egui::Panel::bottom("bottom").frame(Frame::new().fill(p.panel).inner_margin(Margin::symmetric(16, 6))).show(ui, |ui| {
            ui.horizontal(|ui| {
                let jobs = self.shared.queue.list();
                let active: Vec<_> = jobs.iter().filter(|j| matches!(j.state, JobState::Running | JobState::Paused)).collect();
                if let Some(j) = active.first() {
                    let f = if j.progress.bytes_total > 0 { j.progress.bytes_done as f32 / j.progress.bytes_total as f32 } else { 0.0 };
                    let (rect, resp) = ui.allocate_exact_size(vec2(160.0, 8.0), egui::Sense::click());
                    ui.painter().rect_filled(rect, 4.0, p.card);
                    let mut fill = rect;
                    fill.set_width(rect.width() * f);
                    ui.painter().rect_filled(fill, 4.0, p.accent);
                    ui.label(theme::dim(ui, format!("{:.0}% · {} · осталось {}", f * 100.0, util::speed(j.progress.speed), j.progress.eta.map_or("—".into(), util::clock))));
                    if resp.clicked() {
                        self.tab = Tab::Transfer;
                    }
                    ctx.request_repaint_after(Duration::from_millis(100));
                } else {
                    ui.label(theme::dim(ui, "Готово к работе"));
                }
                let staged = self.shared.staging.lock().unwrap().paths.len();
                if staged > 0 {
                    ui.label(theme::dim(ui, format!("· в буфере Strela: {staged}")));
                }
            });
        });
        egui::CentralPanel::default().frame(Frame::new().fill(p.bg).inner_margin(Margin::same(16))).show(ui, |ui| {
            let id = Id::new(("tab-in", self.tab as u8));
            let a = theme::appear(ui, id);
            ui.set_opacity(a);
            ui.add_space((1.0 - a) * 6.0);
            match self.tab {
                Tab::Search => self.search_tab(ui),
                Tab::Transfer => self.transfer.ui(ui, &self.shared),
                Tab::Cleanup => self.cleanup.ui(ui, &self.shared, &mut self.icons),
                Tab::Content => self.content.ui(ui, &self.shared, &mut self.icons),
                Tab::Settings => {
                    let st = self.status.lock().unwrap().clone();
                    if self.prefs.ui(ui, &self.shared, &st) {
                        theme::install(&ctx, &self.shared.settings());
                    }
                }
            }
        });
    }

    fn search_tab(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.main_search.query)
                        .hint_text("Имя файла…   ext:mp4  size:>1GB  modified:<7d  path:D:\\Games")
                        .font(egui::FontId::proportional(18.0))
                        .desired_width(ui.available_width() - 190.0)
                        .margin(vec2(12.0, 9.0)),
                );
                if self.main_search.focus || self.focus_search_on_tab {
                    r.request_focus();
                    self.main_search.focus = false;
                    self.focus_search_on_tab = false;
                }
                let mut s = self.shared.settings();
                let (mut regex, mut fuzzy) = (s.regex, s.fuzzy);
                ui.checkbox(&mut fuzzy, "нечётко");
                ui.checkbox(&mut regex, "regex");
                if regex != s.regex || fuzzy != s.fuzzy {
                    s.regex = regex;
                    s.fuzzy = fuzzy;
                    s.save();
                    *self.shared.settings.write().unwrap() = s;
                }
            });
            ui.horizontal(|ui| {
                ui.label(theme::dim(ui, self.main_search.summary()));
                if self.main_search.busy {
                    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                }
            });
        });
        ui.add_space(8.0);
        self.main_search.keys(ui.ctx());
        self.main_search.list_ui(ui, &mut self.icons, 36.0, None);
    }

    fn spotlight_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let p = pal(ui);
        let frame = Frame::new().fill(p.card).stroke(egui::Stroke::new(1.0, p.border)).corner_radius(18).inner_margin(Margin::same(14));
        egui::CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            frame.show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.horizontal(|ui| {
                    if let Some(l) = &self.logo {
                        ui.add(egui::Image::new(l).fit_to_exact_size(vec2(30.0, 30.0)));
                    }
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.spot_search.query)
                            .hint_text("Поиск файлов во всей системе…")
                            .font(egui::FontId::proportional(22.0))
                            .frame(egui::Frame::NONE)
                            .desired_width(ui.available_width())
                            .margin(vec2(6.0, 6.0)),
                    );
                    if self.spot_search.focus || ctx.input(|i| i.focused) && !r.has_focus() && self.frame_no < 6 {
                        r.request_focus();
                        self.spot_search.focus = false;
                    }
                });
                let n = self.spot_search.rows.len().min(8);
                if n > 0 {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(2.0);
                }
                if self.spot_search.keys(&ctx) {
                    self.leave_spotlight(&ctx, true);
                    return;
                }
                if self.spot_search.list_ui(ui, &mut self.icons, 40.0, Some(8)) {
                    self.leave_spotlight(&ctx, true);
                    return;
                }
                if n > 0 {
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                        ui.label(theme::dim(ui, format!("Enter — открыть · Ctrl+Enter — папка · ПКМ — действия Strela · {}", self.spot_search.summary())));
                    });
                }
            });
        });
        let rows = self.spot_search.rows.len().min(8);
        self.place_spotlight(&ctx, rows);
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.born.elapsed() < Duration::from_secs(5));
        let p = {
            let dark = ctx.global_style().visuals.dark_mode;
            let c = ctx.global_style().visuals.selection.stroke.color;
            theme::palette(dark, [c.r(), c.g(), c.b()])
        };
        for (i, t) in self.toasts.iter().enumerate() {
            let a = (1.0 - (t.born.elapsed().as_secs_f32() - 4.0).max(0.0)).clamp(0.0, 1.0);
            egui::Area::new(Id::new(("toast", i, t.born)))
                .anchor(egui::Align2::RIGHT_BOTTOM, vec2(-18.0, -44.0 - i as f32 * 54.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    ui.set_opacity(a);
                    Frame::new()
                        .fill(p.card)
                        .stroke(egui::Stroke::new(1.0, if t.error { p.err } else { p.accent }))
                        .corner_radius(12)
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&t.text).color(if t.error { p.err } else { p.text }));
                        });
                });
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn dialogs_ui(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.delete {
            match d.ui(ctx, &self.shared) {
                dialogs::Outcome::Open => {}
                dialogs::Outcome::Closed => self.delete = None,
                dialogs::Outcome::Deleted(gone) => {
                    self.delete = None;
                    if !gone.is_empty() {
                        self.cleanup.after_delete(&gone);
                        self.main_search.index_stamp += 1; // the file watcher needs a moment; rerun the query
                        self.toast(format!("Удалено: {}", gone.len()), false);
                    }
                }
            }
        }
        if let Some(d) = &mut self.pick
            && let Some(started) = d.ui(ctx, &self.shared)
        {
            let op = d.op;
            self.pick = None;
            if started {
                self.tab = Tab::Transfer;
                self.toast(if op == Op::Move { "Перемещение добавлено в очередь" } else { "Копирование добавлено в очередь" }, false);
            }
        }
    }

    fn global_keys(&mut self, ctx: &egui::Context) {
        let (ctrl_f, tabs) = ctx.input_mut(|i| {
            let f = i.consume_key(Modifiers::CTRL, Key::F);
            let mut t = None;
            for (n, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5].iter().enumerate() {
                if i.consume_key(Modifiers::CTRL, *k) {
                    t = Some(Tab::ALL[n]);
                }
            }
            (f, t)
        });
        if ctrl_f {
            self.tab = Tab::Search;
            self.main_search.focus = true;
        }
        if let Some(t) = tabs {
            self.tab = t;
        }
    }
}

fn fmt_count(n: usize) -> String {
    if n >= 1_000_000 { format!("{:.1} млн", n as f64 / 1e6) } else if n >= 1000 { format!("{} тыс.", n / 1000) } else { n.to_string() }
}

impl eframe::App for App {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame_no += 1;
        if self.frame_no == 1 {
            self.shared.main_hwnd.store(crate::shell::find_main_window(), std::sync::atomic::Ordering::Relaxed);
        }
        if self.start_hidden && self.frame_no >= 2 && !self.geometry_pending {
            // see main.rs: the off-screen 8x8 window of a hidden start is hidden once its first frames are out
            crate::shell::hide_main(&self.shared);
            self.geometry_pending = true;
            return;
        }
        if self.geometry_pending && self.frame_no > 2 {
            self.geometry_pending = false;
            self.start_hidden = false;
            let mon = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(vec2(900.0, 560.0)));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(1180.0, 760.0)));
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(((mon.x - 1196.0) / 2.0).max(0.0), ((mon.y - 800.0) / 2.0).max(0.0))));
        }
        // closing the window hides it to the tray
        if ctx.input(|i| i.viewport().close_requested()) {
            match self.screen {
                Screen::Main | Screen::Spotlight if self.shared.settings().close_to_tray && !self.shared.quitting.load(std::sync::atomic::Ordering::Relaxed) => {
                    ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                    if matches!(self.screen, Screen::Spotlight) {
                        self.leave_spotlight(&ctx, true);
                    } else {
                        self.hide(&ctx);
                    }
                }
                Screen::Main | Screen::Spotlight => self.shared.quit(),
                _ => {}
            }
        }
        match &mut self.screen {
            Screen::Install(_) | Screen::Uninstall(_) => {
                installer::ui(self, ui);
                return;
            }
            _ => {}
        }
        self.handle_events(&ctx);
        if self.prev_tab != Some(self.tab) {
            self.prev_tab = Some(self.tab);
            self.focus_search_on_tab = self.tab == Tab::Search;
            self.content.want_focus = self.tab == Tab::Content;
        }
        let stamp: u64 = self.status.lock().unwrap().roots.iter().map(|r| r.entries as u64).sum();
        if stamp != self.index_stamp {
            self.index_stamp = stamp;
            self.main_search.index_stamp = stamp;
            self.spot_search.index_stamp = stamp;
        }
        self.main_search.poll();
        self.spot_search.poll();
        match self.screen {
            Screen::Spotlight => {
                // Esc or a click elsewhere closes the popup
                let focused_now = ctx.input(|i| i.viewport().focused);
                if ctx.input(|i| i.key_pressed(Key::Escape)) || (focused_now == Some(false) && self.spot_shown.elapsed() > Duration::from_millis(500)) {
                    self.leave_spotlight(&ctx, true);
                } else {
                    self.spotlight_ui(ui);
                }
            }
            _ => {
                self.global_keys(&ctx);
                self.main_ui(ui);
            }
        }
        self.dialogs_ui(&ctx);
        self.toasts_ui(&ctx);
        let _ = (PathBuf::new(), Color32::WHITE);
    }
}
