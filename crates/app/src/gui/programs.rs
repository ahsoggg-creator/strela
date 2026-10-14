//! "Программы": installed programs, fast removal (quiet, normal or forced) and the cleanup of everything the program
//! leaves behind — with a review list before anything is deleted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Id, Key, RichText, Sense, Ui, pos2, vec2};
use strela_core::apps::{self, App, Mode, Report};
use strela_core::leftovers::{self, CleanReport, Confidence, Subject, Trace};
use strela_core::progress::Progress;
use strela_core::util;
use strela_core::winapi;

use super::table::{self, Col, Sort, col, rcol};
use super::theme::{self, pal};
use super::actions;
use crate::state::Shared;

const COLS: [Col; 5] = [col("Название", 0.0), col("Издатель", 190.0), col("Версия", 120.0), rcol("Размер", 90.0), col("Установлено", 100.0)];

enum Msg {
    Log(String),
    Uninstalled(Result<Report, String>),
    Scanned(Vec<Trace>),
    Cleaned(CleanReport),
}

enum Stage {
    Choose { mode: Mode, scan_after: bool },
    AskName { name: String },
    Working { what: &'static str, log: Vec<String>, started: Instant },
    Review { traces: Vec<Trace>, checked: Vec<bool> },
    Done { lines: Vec<String>, ok: bool, backup: Option<PathBuf> },
}

struct Flow {
    subject: Subject,
    app: Option<App>,
    stage: Stage,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    progress: Arc<Progress>,
    summary: Vec<String>,
    changed: bool,
    scan_after: bool,
    /// Set when the scan follows an uninstall: reliable traces are removed right away, doubtful ones are left for review.
    auto_clean: bool,
    pending: Vec<Trace>,
}

impl Flow {
    fn new(subject: Subject, app: Option<App>, stage: Stage) -> Flow {
        let (tx, rx) = channel();
        Flow { subject, app, stage, tx, rx, progress: Arc::new(Progress::default()), summary: Vec::new(), changed: false, scan_after: true, auto_clean: false, pending: Vec::new() }
    }

    fn start_uninstall(&mut self, ctx: &egui::Context, mode: Mode, scan_after: bool) {
        let Some(app) = self.app.clone() else { return };
        self.progress = Arc::new(Progress::default());
        let (tx, progress, ctx) = (self.tx.clone(), self.progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let txl = tx.clone();
            let c2 = ctx.clone();
            let r = apps::uninstall(&app, mode, &progress, &mut |s| {
                let _ = txl.send(Msg::Log(s.to_string()));
                c2.request_repaint();
            });
            let _ = tx.send(Msg::Uninstalled(r.map_err(|e| e.to_string())));
            ctx.request_repaint();
        });
        self.stage = Stage::Working { what: "Удаление программы", log: Vec::new(), started: Instant::now() };
        self.changed = true;
        self.scan_after = scan_after;
        self.auto_clean = scan_after;
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        self.progress = Arc::new(Progress::default());
        let (tx, progress, ctx, subject) = (self.tx.clone(), self.progress.clone(), ctx.clone(), self.subject.clone());
        std::thread::spawn(move || {
            let r = leftovers::scan(&subject, &progress);
            let _ = tx.send(Msg::Scanned(r));
            ctx.request_repaint();
        });
        self.stage = Stage::Working { what: "Поиск следов в файлах, реестре, автозагрузке, службах и задачах", log: Vec::new(), started: Instant::now() };
    }

    fn start_clean(&mut self, ctx: &egui::Context, traces: Vec<Trace>) {
        self.progress = Arc::new(Progress::default());
        let (tx, progress, ctx, name) = (self.tx.clone(), self.progress.clone(), ctx.clone(), self.subject.name.clone());
        std::thread::spawn(move || {
            let r = leftovers::clean(&traces, &name, &progress);
            let _ = tx.send(Msg::Cleaned(r));
            ctx.request_repaint();
        });
        self.stage = Stage::Working { what: "Удаление следов", log: Vec::new(), started: Instant::now() };
        self.changed = true;
    }

    fn done(&mut self, mut lines: Vec<String>, ok: bool, backup: Option<PathBuf>) {
        let mut all = std::mem::take(&mut self.summary);
        all.append(&mut lines);
        self.stage = Stage::Done { lines: all, ok, backup };
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Log(s) => {
                    if let Stage::Working { log, .. } = &mut self.stage {
                        log.push(s);
                    }
                }
                Msg::Uninstalled(Err(e)) => self.done(vec![format!("Не удалось удалить: {e}")], false, None),
                Msg::Uninstalled(Ok(rep)) => {
                    self.summary.extend(describe(&rep));
                    if self.scan_after {
                        self.start_scan(ctx);
                    } else {
                        let ok = !rep.still_registered;
                        self.done(vec![], ok, None);
                    }
                }
                Msg::Scanned(traces) => {
                    if traces.is_empty() {
                        self.done(vec!["Следов не найдено — система чистая.".into()], true, None);
                    } else if self.auto_clean {
                        self.auto_clean = false;
                        let (high, rest): (Vec<Trace>, Vec<Trace>) = traces.into_iter().partition(|t| t.confidence == Confidence::High);
                        self.pending = rest;
                        if high.is_empty() {
                            let rest = std::mem::take(&mut self.pending);
                            let checked = vec![false; rest.len()];
                            self.stage = Stage::Review { traces: rest, checked };
                        } else {
                            self.start_clean(ctx, high);
                        }
                    } else {
                        let checked = traces.iter().map(|t| t.confidence == Confidence::High).collect();
                        self.stage = Stage::Review { traces, checked };
                    }
                }
                Msg::Cleaned(rep) => {
                    let mut lines = vec![format!("Удалено объектов: {} ({}) за {:.2} с", rep.removed, util::bytes(rep.bytes), rep.elapsed.as_secs_f64())];
                    for (what, why) in rep.failed.iter().take(8) {
                        lines.push(format!("Не удалось: {what} — {why}"));
                    }
                    if rep.failed.len() > 8 {
                        lines.push(format!("…и ещё {}", rep.failed.len() - 8));
                    }
                    if rep.backup.is_some() {
                        lines.push("Разделы реестра перед удалением сохранены в .reg-файлы — двойной щелчок по файлу вернёт их.".into());
                    }
                    let ok = rep.failed.is_empty();
                    if self.pending.is_empty() {
                        self.done(lines, ok, rep.backup);
                    } else {
                        self.summary.extend(lines);
                        let rest = std::mem::take(&mut self.pending);
                        let checked = vec![false; rest.len()];
                        self.stage = Stage::Review { traces: rest, checked };
                    }
                }
            }
        }
    }

    /// Returns `Some(changed)` when the dialog is closed.
    fn ui(&mut self, ctx: &egui::Context, shared: &Arc<Shared>) -> Option<bool> {
        self.poll(ctx);
        let p = {
            let dark = ctx.global_style().visuals.dark_mode;
            let c = ctx.global_style().visuals.selection.stroke.color;
            theme::palette(dark, [c.r(), c.g(), c.b()])
        };
        enum Act {
            None,
            Close,
            Uninstall(Mode, bool),
            Scan,
            Clean(Vec<Trace>),
            Named(String),
        }
        let mut act = Act::None;
        let title = match &self.stage {
            Stage::AskName { .. } => "Следы удалённой программы".to_string(),
            _ => self.subject.name.clone(),
        };
        egui::Modal::new(Id::new("programs-flow")).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.label(RichText::new(title).size(19.0).strong());
            ui.add_space(6.0);
            match &mut self.stage {
                Stage::AskName { name } => {
                    ui.label("Введите название программы, которую вы уже удалили. Strela найдёт то, что от неё осталось, и покажет список перед удалением.");
                    ui.add_space(6.0);
                    let r = ui.add(egui::TextEdit::singleline(name).hint_text("например: Notepad++").desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)));
                    r.request_focus();
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let go = !name.trim().is_empty();
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if ui.add_enabled(go, egui::Button::new(RichText::new("Искать следы").strong().color(egui::Color32::WHITE)).fill(p.accent).min_size(vec2(0.0, 32.0))).clicked() || (enter && go) {
                            act = Act::Named(name.trim().to_string());
                        }
                        if ui.button("Отмена").clicked() {
                            act = Act::Close;
                        }
                    });
                }
                Stage::Choose { mode, scan_after } => {
                    let app = self.app.as_ref().expect("app for uninstall");
                    ui.label(theme::dim(ui, format!("{} · {} · {}", if app.version.is_empty() { "—" } else { &app.version }, if app.publisher.is_empty() { "издатель не указан" } else { &app.publisher }, app.scope())));
                    if let Some(d) = &app.install_location {
                        ui.label(theme::dim(ui, format!("Папка: {}", winapi::display_path(d))));
                    }
                    ui.add_space(8.0);
                    let quiet_note = if app.msi_guid.is_some() {
                        "msiexec без окон — быстро и надёжно"
                    } else if app.quiet_uninstall.is_some() {
                        "родной деинсталлятор без окон"
                    } else {
                        "у программы нет тихого режима — откроется её окно удаления"
                    };
                    let force_note = if app.msi_guid.is_some() { "для программ Windows Installer всё равно используется msiexec без окон" } else if app.install_location.is_some() { "закроет процессы и службы программы, мгновенно удалит её папку и запись из списка — без деинсталлятора" } else { "папка установки неизвестна: удалится только запись о программе, остальное найдёт поиск следов" };
                    if app.store.is_some() {
                        ui.label("Приложение Microsoft Store будет удалено для текущего пользователя вместе с его данными — без окон и деинсталлятора.");
                    } else {
                        for (m, name, note) in [
                            (Mode::Quiet, "Обычное удаление, без окон", quiet_note),
                            (Mode::Interactive, "Обычное удаление, с окнами деинсталлятора", "как через «Программы и компоненты»"),
                            (Mode::Force, "Принудительное, мгновенное", force_note),
                        ] {
                            ui.radio_value(mode, m, RichText::new(name).strong());
                            ui.label(theme::dim(ui, format!("      {note}")));
                            ui.add_space(2.0);
                        }
                    }
                    ui.add_space(6.0);
                    ui.checkbox(scan_after, "После удаления сразу очистить следы (надёжные — автоматически, сомнительные — покажу на проверку)");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::danger_button(ui, "Удалить программу").clicked() {
                            act = Act::Uninstall(*mode, *scan_after);
                        }
                        if ui.button("Только найти следы").clicked() {
                            act = Act::Scan;
                        }
                        if ui.button("Отмена").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            act = Act::Close;
                        }
                    });
                }
                Stage::Working { what, log, started } => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().color(p.accent));
                        ui.label(format!("{what}… {:.1} с", started.elapsed().as_secs_f64()));
                    });
                    for l in log.iter().rev().take(6).rev() {
                        ui.add(egui::Label::new(theme::dim(ui, l.as_str())).truncate());
                    }
                    ui.add_space(6.0);
                    if ui.button("Остановить").clicked() {
                        self.progress.cancel();
                    }
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Stage::Review { traces, checked } => {
                    for l in &self.summary {
                        ui.label(theme::dim(ui, l.as_str()));
                    }
                    review_ui(ui, traces, checked, &p);
                    ui.add_space(8.0);
                    let n = checked.iter().filter(|c| **c).count();
                    let size: u64 = traces.iter().zip(checked.iter()).filter(|(_, c)| **c).map(|(t, _)| t.size).sum();
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(n > 0, |ui| {
                            if theme::danger_button(ui, &format!("Удалить выбранное ({n}, {})", util::bytes(size))).clicked() {
                                let sel: Vec<Trace> = traces.iter().zip(checked.iter()).filter(|(_, c)| **c).map(|(t, _)| t.clone()).collect();
                                act = Act::Clean(sel);
                            }
                        });
                        if ui.button("Закрыть, ничего не удаляя").clicked() {
                            act = Act::Close;
                        }
                    });
                    ui.label(theme::dim(ui, "Реестр перед удалением сохраняется в .reg-файлы. Защищённые папки Windows и Program Files не трогаются."));
                }
                Stage::Done { lines, ok, backup } => {
                    for (i, l) in lines.iter().enumerate() {
                        let color = if l.starts_with("Не удалось") || (i == 0 && !*ok) { p.err } else { p.text };
                        ui.label(RichText::new(l.as_str()).color(color));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Закрыть").clicked() || ui.input(|i| i.key_pressed(Key::Escape) || i.key_pressed(Key::Enter)) {
                            act = Act::Close;
                        }
                        if let Some(b) = backup
                            && ui.button("Открыть папку с копиями реестра").clicked()
                        {
                            actions::open_path(&b.to_string_lossy());
                        }
                    });
                }
            }
        });
        let _ = shared;
        match act {
            Act::None => None,
            Act::Close => {
                self.progress.cancel();
                Some(self.changed)
            }
            Act::Uninstall(m, s) => {
                self.start_uninstall(ctx, m, s);
                None
            }
            Act::Scan => {
                self.start_scan(ctx);
                None
            }
            Act::Clean(t) => {
                self.start_clean(ctx, t);
                None
            }
            Act::Named(n) => {
                self.subject = Subject::by_name(&n);
                self.start_scan(ctx);
                None
            }
        }
    }
}

fn describe(rep: &Report) -> Vec<String> {
    let mut l = Vec::new();
    match rep.uninstaller_exit {
        Some(0) => l.push("Деинсталлятор завершился успешно.".to_string()),
        Some(c) => l.push(format!("Деинсталлятор завершился с кодом {c}.")),
        None => {}
    }
    if rep.killed > 0 {
        l.push(format!("Закрыто процессов программы: {}.", rep.killed));
    }
    if rep.services_removed > 0 {
        l.push(format!("Удалено служб: {}.", rep.services_removed));
    }
    if let Some(d) = &rep.folder_removed {
        l.push(format!("Папка {} удалена ({}) за {:.2} с.", winapi::display_path(d), util::bytes(rep.bytes), rep.elapsed.as_secs_f64()));
    }
    l.extend(rep.notes.iter().cloned());
    if rep.still_registered {
        l.push("Запись о программе всё ещё в списке.".into());
    }
    l
}

fn review_ui(ui: &mut Ui, traces: &[Trace], checked: &mut [bool], p: &theme::Palette) {
    let (high, medium) = (traces.iter().filter(|t| t.confidence == Confidence::High).count(), traces.iter().filter(|t| t.confidence == Confidence::Medium).count());
    ui.label(format!("Найдено следов: {} — надёжных {high}, вероятных {medium}.", traces.len()));
    ui.label(theme::dim(ui, "Надёжные отмечены сразу. Вероятные совпадают только частью названия — проверьте их глазами."));
    ui.horizontal(|ui| {
        if ui.small_button("Только надёжные").clicked() {
            for (c, t) in checked.iter_mut().zip(traces) {
                *c = t.confidence == Confidence::High;
            }
        }
        if ui.small_button("Все").clicked() {
            checked.iter_mut().for_each(|c| *c = true);
        }
        if ui.small_button("Снять все").clicked() {
            checked.iter_mut().for_each(|c| *c = false);
        }
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical().max_height(340.0).auto_shrink([false, true]).show(ui, |ui| {
        for (i, t) in traces.iter().enumerate() {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
            table::row_bg(ui, rect, resp.hovered(), false);
            if resp.clicked() {
                checked[i] = !checked[i];
            }
            let cb = egui::Rect::from_min_size(pos2(rect.left() + 6.0, rect.center().y - 10.0), vec2(20.0, 20.0));
            ui.put(cb, egui::Checkbox::without_text(&mut checked[i]));
            let painter = ui.painter_at(rect);
            let color = if t.confidence == Confidence::High { p.accent } else { p.warn };
            painter.text(pos2(rect.left() + 36.0, rect.top() + 7.0), egui::Align2::LEFT_TOP, t.kind, egui::FontId::proportional(11.5), color);
            painter.text(pos2(rect.left() + 36.0, rect.top() + 24.0), egui::Align2::LEFT_TOP, &t.reason, egui::FontId::proportional(11.5), p.dim);
            let text = painter.with_clip_rect(egui::Rect::from_min_max(pos2(rect.left() + 150.0, rect.top()), pos2(rect.right() - 80.0, rect.bottom())));
            text.text(pos2(rect.left() + 150.0, rect.center().y), egui::Align2::LEFT_CENTER, &t.label, egui::FontId::proportional(13.0), p.text);
            if t.size > 0 {
                painter.text(pos2(rect.right() - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, util::bytes(t.size), egui::FontId::proportional(12.5), p.dim);
            }
            resp.on_hover_text(&t.label);
        }
    });
}

// ---------------------------------------------------------------------------------------------------------

pub struct ProgramsTab {
    apps: Vec<App>,
    loading: Option<Receiver<Vec<App>>>,
    loaded: bool,
    filter: String,
    sort: Sort,
    selected: Option<strela_core::registry::KeyPath>,
    flow: Option<Flow>,
}

impl ProgramsTab {
    pub fn new() -> ProgramsTab {
        ProgramsTab { apps: Vec::new(), loading: None, loaded: false, filter: String::new(), sort: Sort { col: 0, desc: false }, selected: None, flow: None }
    }

    fn reload(&mut self, ctx: &egui::Context) {
        if self.loading.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(apps::list());
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    fn sorted(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.apps.len()).filter(|&i| {
            let a = &self.apps[i];
            table::matches(&self.filter, &[&a.name, &a.publisher, &a.version])
        }).collect();
        let s = self.sort;
        idx.sort_by(|&x, &y| {
            let (a, b) = (&self.apps[x], &self.apps[y]);
            let o = match s.col {
                1 => a.publisher.to_lowercase().cmp(&b.publisher.to_lowercase()),
                2 => a.version.cmp(&b.version),
                3 => a.size_bytes.cmp(&b.size_bytes),
                4 => a.install_date.cmp(&b.install_date),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if s.desc { o.reverse() } else { o }
        });
        idx
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>) {
        let p = pal(ui);
        if !self.loaded && self.loading.is_none() {
            self.reload(ui.ctx());
        }
        if let Some(rx) = &self.loading
            && let Ok(list) = rx.try_recv()
        {
            self.apps = list;
            self.loading = None;
            self.loaded = true;
            if let Some(sel) = &self.selected
                && !self.apps.iter().any(|a| &a.key == sel)
            {
                self.selected = None;
            }
        }
        if let Some(f) = &mut self.flow
            && let Some(changed) = f.ui(ui.ctx(), shared)
        {
            self.flow = None;
            if changed {
                self.loaded = false;
            }
        }
        super::actions::admin_banner(ui, shared, "programs", "Для программ «для всех пользователей» и их служб нужны права администратора.");
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::heading(ui, "Установленные программы");
                if self.loading.is_some() {
                    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                } else {
                    ui.label(theme::dim(ui, format!("{} шт.", self.apps.len())));
                }
            });
            ui.label(theme::dim(ui, "Удаление в один клик — тихо или мгновенно, с поиском и очисткой всего, что программа оставила в системе."));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                table::filter_box(ui, &mut self.filter, "Поиск по названию или издателю", 360.0);
                if ui.button("Обновить").clicked() {
                    self.loaded = false;
                }
                if ui.button("Следы уже удалённой программы…").clicked() {
                    self.flow = Some(Flow::new(Subject::by_name(""), None, Stage::AskName { name: String::new() }));
                }
            });
        });
        ui.add_space(8.0);
        let idx = self.sorted();
        let bar_h = 74.0;
        table::header(ui, &COLS, &mut self.sort);
        let list_h = (ui.available_height() - bar_h - 8.0).max(120.0);
        let row_h = 38.0;
        let mut clicked: Option<usize> = None;
        let mut ctx_action: Option<(usize, u8)> = None;
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show_rows(ui, row_h, idx.len(), |ui, range| {
            for r in range {
                let i = idx[r];
                let a = &self.apps[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                let sel = self.selected.as_ref() == Some(&a.key);
                table::row_bg(ui, rect, resp.hovered(), sel);
                let xs = table::layout(rect.width(), &COLS);
                table::cell(ui, rect, &COLS[0], xs[0], &a.name, p.text, 14.0);
                table::cell(ui, rect, &COLS[1], xs[1], &a.publisher, p.dim, 12.5);
                table::cell(ui, rect, &COLS[2], xs[2], &a.version, p.dim, 12.5);
                table::cell(ui, rect, &COLS[3], xs[3], &if a.size_bytes > 0 { util::bytes(a.size_bytes) } else { String::new() }, p.dim, 12.5);
                table::cell(ui, rect, &COLS[4], xs[4], &a.install_date, p.dim, 12.5);
                if resp.clicked() {
                    clicked = Some(i);
                }
                if resp.double_clicked()
                    && let Some(d) = &a.install_location
                {
                    actions::open_path(&d.to_string_lossy());
                }
                resp.context_menu(|ui| {
                    clicked = Some(i);
                    if ui.button("Удалить…").clicked() {
                        ctx_action = Some((i, 0));
                        ui.close();
                    }
                    if ui.button("Найти следы").clicked() {
                        ctx_action = Some((i, 1));
                        ui.close();
                    }
                    if let Some(d) = &a.install_location
                        && ui.button("Открыть папку установки").clicked()
                    {
                        actions::open_path(&d.to_string_lossy());
                        ui.close();
                    }
                    if ui.button("Копировать название").clicked() {
                        ui.ctx().copy_text(a.name.clone());
                        ui.close();
                    }
                });
            }
        });
        if let Some(i) = clicked {
            self.selected = Some(self.apps[i].key.clone());
        }
        ui.add_space(6.0);
        let sel_app = self.selected.as_ref().and_then(|k| self.apps.iter().find(|a| &a.key == k)).cloned();
        let mut open: Option<(App, bool)> = None;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match &sel_app {
                None => {
                    ui.label(theme::dim(ui, "Выберите программу в списке."));
                }
                Some(a) => {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&a.name).strong().size(15.0));
                            let loc = a.install_location.as_ref().map_or("папка установки неизвестна".to_string(), |d| winapi::display_path(d));
                            ui.add(egui::Label::new(theme::dim(ui, format!("{}{}", loc, if a.is_32bit() { " · 32-бит" } else { "" }))).truncate());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::danger_button(ui, "Удалить…").clicked() {
                                open = Some((a.clone(), false));
                            }
                            if ui.add(egui::Button::new("Найти следы").min_size(vec2(0.0, 32.0))).clicked() {
                                open = Some((a.clone(), true));
                            }
                        });
                    });
                }
            }
        });
        if let Some((i, which)) = ctx_action {
            open = Some((self.apps[i].clone(), which == 1));
        }
        if let Some((a, scan_only)) = open {
            let mut f = Flow::new(Subject::from_app(&a), Some(a), Stage::Choose { mode: Mode::Quiet, scan_after: true });
            if scan_only {
                f.start_scan(ui.ctx());
            }
            self.flow = Some(f);
        }
    }
}
