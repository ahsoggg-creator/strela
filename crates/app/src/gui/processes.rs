//! "Процессы": a live process table (CPU, memory, I/O, handles, threads, owner), a process tree, details with the
//! command line and loaded modules, and the actions — end, end the whole tree, suspend / resume, priority.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Id, Key, RichText, Sense, Ui, pos2, vec2};
use strela_core::procs::{self, ModuleInfo, Priority, ProcInfo, Sampler};
use strela_core::util;

use super::actions;
use super::table::{self, Col, Sort, col, rcol};
use super::theme::{self, pal};
use crate::state::Shared;

const COLS: [Col; 11] = [
    col("Процесс", 0.0),
    rcol("PID", 66.0),
    rcol("ЦП", 62.0),
    rcol("Память", 92.0),
    rcol("Частная", 92.0),
    rcol("Потоки", 64.0),
    rcol("Дескр.", 70.0),
    rcol("Ввод-вывод", 100.0),
    col("Пользователь", 130.0),
    col("Подпись", 140.0),
    col("Описание", 0.0),
];

struct Live {
    procs: Mutex<Vec<ProcInfo>>,
    stamp: AtomicU64,
    last_seen: AtomicU64,
    own_pid: AtomicU64,
    stop: AtomicBool,
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 10_000
}

fn spawn_sampler(live: Arc<Live>, ctx: egui::Context) {
    std::thread::Builder::new()
        .name("strela-procs".into())
        .spawn(move || {
            let mut s = Sampler::new();
            live.own_pid.store(s.own_pid() as u64, Relaxed);
            loop {
                if live.stop.load(Relaxed) {
                    return;
                }
                // sample only while the tab is on screen
                if now_ms().saturating_sub(live.last_seen.load(Relaxed)) > 2500 {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                if let Ok(v) = s.snapshot() {
                    *live.procs.lock().unwrap() = v;
                    live.stamp.fetch_add(1, Relaxed);
                    ctx.request_repaint();
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        })
        .expect("spawn process sampler");
}

struct Kill {
    pids: Vec<u32>,
    title: String,
    tree: bool,
}

struct Detail {
    pid: u32,
    start: u64,
    cmdline: Option<String>,
    modules: Option<Vec<ModuleInfo>>,
}

pub struct ProcessesTab {
    live: Option<Arc<Live>>,
    list: Vec<ProcInfo>,
    seen_stamp: u64,
    filter: String,
    sort: Sort,
    tree: bool,
    collapsed: HashSet<u32>,
    selected: Option<u32>,
    detail: Option<Detail>,
    show_modules: bool,
    kill: Option<Kill>,
    message: Option<(String, bool)>,
}

impl ProcessesTab {
    pub fn new() -> ProcessesTab {
        ProcessesTab {
            live: None,
            list: Vec::new(),
            seen_stamp: 0,
            filter: String::new(),
            sort: Sort { col: 2, desc: true },
            tree: false,
            collapsed: HashSet::new(),
            selected: None,
            detail: None,
            show_modules: false,
            kill: None,
            message: None,
        }
    }

    fn own_pid(&self) -> u32 {
        self.live.as_ref().map_or(0, |l| l.own_pid.load(Relaxed) as u32)
    }

    fn sort_cmp(&self, a: &ProcInfo, b: &ProcInfo) -> std::cmp::Ordering {
        let o = match self.sort.col {
            1 => a.pid.cmp(&b.pid),
            2 => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
            3 => a.working_set.cmp(&b.working_set),
            4 => a.private_bytes.cmp(&b.private_bytes),
            5 => a.threads.cmp(&b.threads),
            6 => a.handles.cmp(&b.handles),
            7 => (a.io_read + a.io_write).cmp(&(b.io_read + b.io_write)),
            8 => a.user.as_deref().unwrap_or("").to_lowercase().cmp(&b.user.as_deref().unwrap_or("").to_lowercase()),
            9 => table::trust_rank(a.path.as_deref()).cmp(&table::trust_rank(b.path.as_deref())),
            10 => a.description.as_deref().unwrap_or("").to_lowercase().cmp(&b.description.as_deref().unwrap_or("").to_lowercase()),
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        };
        if self.sort.desc { o.reverse() } else { o }
    }

    /// Rows to show: (index into `list`, depth, has children).
    fn rows(&self) -> Vec<(usize, u32, bool)> {
        let filtering = !self.filter.trim().is_empty();
        if !self.tree || filtering {
            let mut v: Vec<usize> = (0..self.list.len())
                .filter(|&i| {
                    let p = &self.list[i];
                    !filtering || table::matches(&self.filter, &[&p.name, &p.pid.to_string(), p.user.as_deref().unwrap_or(""), p.description.as_deref().unwrap_or(""), p.company.as_deref().unwrap_or(""), &p.path.as_ref().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()])
                })
                .collect();
            v.sort_by(|&a, &b| self.sort_cmp(&self.list[a], &self.list[b]));
            return v.into_iter().map(|i| (i, 0, false)).collect();
        }
        let pids: HashMap<u32, usize> = self.list.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
        let mut kids: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut roots = Vec::new();
        for (i, p) in self.list.iter().enumerate() {
            // a parent that is gone, itself, or started later (pid reuse) makes this a root
            let parent_ok = p.ppid != p.pid && pids.get(&p.ppid).is_some_and(|&pi| self.list[pi].start_unix <= p.start_unix || p.start_unix.is_none());
            if parent_ok {
                kids.entry(p.ppid).or_default().push(i);
            } else {
                roots.push(i);
            }
        }
        for v in kids.values_mut() {
            v.sort_by(|&a, &b| self.sort_cmp(&self.list[a], &self.list[b]));
        }
        roots.sort_by(|&a, &b| self.sort_cmp(&self.list[a], &self.list[b]));
        let mut out = Vec::with_capacity(self.list.len());
        let mut stack: Vec<(usize, u32)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
        let mut seen = HashSet::new();
        while let Some((i, d)) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            let pid = self.list[i].pid;
            let ch = kids.get(&pid);
            out.push((i, d, ch.is_some()));
            if !self.collapsed.contains(&pid)
                && let Some(ch) = ch
            {
                for &c in ch.iter().rev() {
                    stack.push((c, d + 1));
                }
            }
        }
        out
    }

    fn select(&mut self, pid: u32) {
        self.selected = Some(pid);
        let start = self.list.iter().find(|p| p.pid == pid).and_then(|p| p.start_unix).unwrap_or(0);
        self.detail = Some(Detail { pid, start, cmdline: procs::command_line(pid), modules: None });
        if self.show_modules {
            self.load_modules();
        }
    }

    fn load_modules(&mut self) {
        if let Some(d) = &mut self.detail {
            d.modules = Some(procs::modules(d.pid).unwrap_or_default());
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>) {
        let p = pal(ui);
        let live = self
            .live
            .get_or_insert_with(|| {
                let l = Arc::new(Live { procs: Mutex::new(Vec::new()), stamp: AtomicU64::new(0), last_seen: AtomicU64::new(0), own_pid: AtomicU64::new(0), stop: AtomicBool::new(false) });
                spawn_sampler(l.clone(), ui.ctx().clone());
                l
            })
            .clone();
        live.last_seen.store(now_ms(), Relaxed);
        let stamp = live.stamp.load(Relaxed);
        if stamp != self.seen_stamp {
            self.seen_stamp = stamp;
            self.list = live.procs.lock().unwrap().clone();
            if let Some(sel) = self.selected
                && !self.list.iter().any(|p| p.pid == sel)
            {
                self.selected = None;
                self.detail = None;
            }
        }
        ui.ctx().request_repaint_after(Duration::from_millis(1000));
        self.kill_dialog(ui.ctx());
        super::actions::admin_banner(ui, shared, "processes", "Без прав администратора нельзя завершать и менять процессы других пользователей и служб.");
        let total_cpu: f32 = self.list.iter().filter(|p| p.pid != 0).map(|p| p.cpu).sum::<f32>().min(100.0);
        let total_mem: u64 = self.list.iter().map(|p| p.working_set).sum();
        let threads: u32 = self.list.iter().map(|p| p.threads).sum();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::heading(ui, "Процессы");
                ui.label(theme::dim(ui, format!("{} процессов · {threads} потоков · ЦП {total_cpu:.0}% · память процессов {}", self.list.len(), util::bytes(total_mem))));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                table::filter_box(ui, &mut self.filter, "Имя, PID, пользователь, путь…", 320.0);
                ui.checkbox(&mut self.tree, "деревом");
                if let Some((m, err)) = &self.message {
                    ui.colored_label(if *err { p.err } else { p.ok }, m);
                }
            });
        });
        ui.add_space(8.0);
        table::repaint_while_checking(ui);
        let rows = self.rows();
        table::header(ui, &COLS, &mut self.sort);
        let details_h = if self.show_modules && self.selected.is_some() { 300.0 } else { 172.0 };
        let list_h = (ui.available_height() - details_h - 8.0).max(120.0);
        let row_h = 30.0;
        let mut clicked: Option<u32> = None;
        let mut toggled: Option<u32> = None;
        let mut menu_kill: Option<(u32, bool)> = None;
        let own = self.own_pid();
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            for r in range {
                let (i, depth, has_kids) = rows[r];
                let pr = &self.list[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                table::row_bg(ui, rect, resp.hovered(), self.selected == Some(pr.pid));
                let xs = table::layout(rect.width(), &COLS);
                // name with tree indent
                let indent = depth as f32 * 16.0;
                if has_kids {
                    let arrow = egui::Rect::from_min_size(pos2(rect.left() + xs[0].0 + indent, rect.top()), vec2(16.0, rect.height()));
                    let ar = ui.interact(arrow, ui.id().with(("arrow", pr.pid)), Sense::click());
                    ui.painter().text(arrow.center(), egui::Align2::CENTER_CENTER, if self.collapsed.contains(&pr.pid) { "▸" } else { "▾" }, egui::FontId::proportional(13.0), p.dim);
                    if ar.clicked() {
                        toggled = Some(pr.pid);
                    }
                }
                let name_pos = (xs[0].0 + indent + 18.0, (xs[0].1 - indent - 18.0).max(40.0));
                let name_color = if !pr.accessible { p.dim } else { p.text };
                table::cell(ui, rect, &COLS[0], name_pos, &pr.name, name_color, 13.5);
                table::cell(ui, rect, &COLS[1], xs[1], &pr.pid.to_string(), p.dim, 12.5);
                if pr.cpu >= 0.05 {
                    let w = (xs[2].1 - 8.0) * (pr.cpu / 100.0).clamp(0.0, 1.0);
                    let bar = egui::Rect::from_min_size(pos2(rect.left() + xs[2].0 + 4.0, rect.bottom() - 5.0), vec2(w.max(2.0), 3.0));
                    ui.painter().rect_filled(bar, 2.0, if pr.cpu > 50.0 { p.err } else if pr.cpu > 15.0 { p.warn } else { p.accent });
                }
                table::cell(ui, rect, &COLS[2], xs[2], &if pr.cpu >= 0.05 { format!("{:.1}%", pr.cpu) } else { "0%".into() }, if pr.cpu > 15.0 { p.warn } else { p.dim }, 12.5);
                table::cell(ui, rect, &COLS[3], xs[3], &if pr.working_set > 0 { util::bytes(pr.working_set) } else { "—".into() }, p.text, 12.5);
                table::cell(ui, rect, &COLS[4], xs[4], &if pr.private_bytes > 0 { util::bytes(pr.private_bytes) } else { "—".into() }, p.dim, 12.5);
                table::cell(ui, rect, &COLS[5], xs[5], &pr.threads.to_string(), p.dim, 12.5);
                table::cell(ui, rect, &COLS[6], xs[6], &if pr.handles > 0 { pr.handles.to_string() } else { "—".into() }, p.dim, 12.5);
                table::cell(ui, rect, &COLS[7], xs[7], &if pr.io_read + pr.io_write > 0 { util::bytes(pr.io_read + pr.io_write) } else { "—".into() }, p.dim, 12.5);
                table::cell(ui, rect, &COLS[8], xs[8], pr.user.as_deref().map_or("", |u| u.rsplit('\\').next().unwrap_or(u)), p.dim, 12.5);
                let (tt, tc) = table::trust_cell(&p, pr.path.as_deref());
                table::cell(ui, rect, &COLS[9], xs[9], &tt, tc, 12.5);
                table::cell(ui, rect, &COLS[10], xs[10], pr.description.as_deref().or(pr.company.as_deref()).unwrap_or(""), p.dim, 12.5);
                if resp.clicked() {
                    clicked = Some(pr.pid);
                }
                let pid = pr.pid;
                let protected = procs::protected_reason(pr, own).is_some();
                resp.context_menu(|ui| {
                    clicked = Some(pid);
                    if ui.add_enabled(!protected, egui::Button::new("Завершить процесс")).clicked() {
                        menu_kill = Some((pid, false));
                        ui.close();
                    }
                    if ui.add_enabled(!protected, egui::Button::new("Завершить дерево процессов")).clicked() {
                        menu_kill = Some((pid, true));
                        ui.close();
                    }
                    if let Some(path) = &pr.path {
                        ui.separator();
                        if ui.button("Показать файл").clicked() {
                            actions::reveal(&path.to_string_lossy());
                            ui.close();
                        }
                        if ui.button("Копировать путь").clicked() {
                            ui.ctx().copy_text(path.to_string_lossy().into_owned());
                            ui.close();
                        }
                    }
                });
            }
        });
        if let Some(pid) = toggled
            && !self.collapsed.remove(&pid)
        {
            self.collapsed.insert(pid);
        }
        if let Some(pid) = clicked {
            self.select(pid);
        }
        if let Some((pid, tree)) = menu_kill {
            self.ask_kill(pid, tree);
        }
        // Delete ends the selected process (not while typing in the filter)
        if self.kill.is_none() && ui.ctx().memory(|m| m.focused().is_none()) && ui.input(|i| i.key_pressed(Key::Delete))
            && let Some(pid) = self.selected
        {
            self.ask_kill(pid, false);
        }
        ui.add_space(6.0);
        self.details(ui);
    }

    fn ask_kill(&mut self, pid: u32, tree: bool) {
        let Some(pr) = self.list.iter().find(|p| p.pid == pid) else { return };
        if let Some(why) = procs::protected_reason(pr, self.own_pid()) {
            self.message = Some((format!("{} ({pid}): нельзя завершить — {why}", pr.name), true));
            return;
        }
        let pids = if tree { procs::tree_pids(pid, &self.list) } else { vec![pid] };
        self.kill = Some(Kill { pids, title: format!("{} (PID {pid})", pr.name), tree });
    }

    fn details(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let own = self.own_pid();
        let mut kill: Option<(u32, bool)> = None;
        let mut want_modules = false;
        let mut set_prio: Option<(u32, Priority)> = None;
        let mut suspend: Option<(u32, bool)> = None;
        let sel = self.selected.and_then(|pid| self.list.iter().find(|p| p.pid == pid)).cloned();
        let show_modules = self.show_modules;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let Some(pr) = &sel else {
                ui.label(theme::dim(ui, "Выберите процесс: здесь появятся путь, командная строка, память, ввод-вывод, модули. Delete — завершить выбранный."));
                ui.set_min_height(100.0);
                return;
            };
            let d = self.detail.as_ref().filter(|d| d.pid == pr.pid);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&pr.name).strong().size(15.0));
                ui.label(theme::dim(ui, format!("PID {} · родитель {}", pr.pid, pr.ppid)));
                if pr.critical {
                    theme::chip(ui, "критический", p.err);
                }
                if pr.wow64 {
                    theme::chip(ui, "32-бит", p.dim);
                }
                if !pr.accessible {
                    theme::chip(ui, "нет доступа", p.warn);
                }
                if let Some(c) = &pr.company {
                    ui.label(theme::dim(ui, format!("· {c}")));
                }
            });
            if let Some(path) = &pr.path {
                ui.add(egui::Label::new(RichText::new(strela_core::winapi::display_path(path)).size(12.5)).truncate());
            }
            if let Some(cl) = d.and_then(|d| d.cmdline.as_ref()) {
                ui.add(egui::Label::new(theme::dim(ui, cl.as_str())).truncate()).on_hover_text(cl);
            }
            let started = d.map(|d| d.start).filter(|s| *s > 0).map(|s| util::date(s as u32)).unwrap_or_default();
            let run_for = pr.start_unix.and_then(|s| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|n| n.as_secs().saturating_sub(s)));
            ui.label(theme::dim(
                ui,
                format!(
                    "Память: {} (пик {}, частная {}) · ввод-вывод: чтение {}, запись {} · дескрипторов {} · потоков {} · ЦП-время {} · {}{}",
                    util::bytes(pr.working_set),
                    util::bytes(pr.peak_working_set),
                    util::bytes(pr.private_bytes),
                    util::bytes(pr.io_read),
                    util::bytes(pr.io_write),
                    pr.handles,
                    pr.threads,
                    util::clock(Duration::from_secs_f64(pr.cpu_seconds)),
                    if started.is_empty() { String::new() } else { format!("запущен {started}") },
                    run_for.map_or(String::new(), |s| format!(" (работает {})", util::clock(Duration::from_secs(s)))),
                ),
            ));
            let reason = procs::protected_reason(pr, own);
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(reason.is_none(), |ui| {
                    if theme::danger_button(ui, "Завершить").clicked() {
                        kill = Some((pr.pid, false));
                    }
                    if ui.add(egui::Button::new("Завершить дерево").min_size(vec2(0.0, 32.0))).clicked() {
                        kill = Some((pr.pid, true));
                    }
                    if ui.button("Приостановить").clicked() {
                        suspend = Some((pr.pid, true));
                    }
                    if ui.button("Возобновить").clicked() {
                        suspend = Some((pr.pid, false));
                    }
                });
                if let Some(r) = reason {
                    ui.label(theme::dim(ui, format!("нельзя завершить: {r}")));
                }
                if let Some(cur) = pr.priority {
                    let mut chosen = cur;
                    egui::ComboBox::from_id_salt("prio").selected_text(format!("Приоритет: {}", cur.title())).show_ui(ui, |ui| {
                        for pr_ in Priority::ALL {
                            ui.selectable_value(&mut chosen, pr_, pr_.title());
                        }
                    });
                    if chosen != cur {
                        set_prio = Some((pr.pid, chosen));
                    }
                }
                if let Some(path) = &pr.path {
                    if ui.button("Показать файл").clicked() {
                        actions::reveal(&path.to_string_lossy());
                    }
                    if ui.button("Копировать путь").clicked() {
                        ui.ctx().copy_text(path.to_string_lossy().into_owned());
                    }
                }
                let mut m = show_modules;
                if ui.toggle_value(&mut m, "Модули (DLL)").changed() {
                    want_modules = true;
                }
            });
            if show_modules {
                match d.and_then(|d| d.modules.as_ref()) {
                    None => {
                        ui.label(theme::dim(ui, "Не удалось получить список модулей (нужны права или процесс защищён)."));
                    }
                    Some(mods) => {
                        ui.label(theme::dim(ui, format!("Модулей: {}", mods.len())));
                        egui::ScrollArea::vertical().id_salt("mods").max_height(120.0).show_rows(ui, 18.0, mods.len(), |ui, range| {
                            for i in range {
                                let m = &mods[i];
                                ui.horizontal(|ui| {
                                    ui.add_sized([180.0, 16.0], egui::Label::new(RichText::new(&m.name).size(12.0)).truncate());
                                    ui.add(egui::Label::new(theme::dim(ui, format!("{}  ·  {}", strela_core::winapi::display_path(&m.path), util::bytes(m.size as u64)))).truncate());
                                });
                            }
                        });
                    }
                }
            }
        });
        if want_modules {
            self.show_modules = !self.show_modules;
            if self.show_modules {
                self.load_modules();
            }
        }
        if let Some((pid, tree)) = kill {
            self.ask_kill(pid, tree);
        }
        if let Some((pid, prio)) = set_prio {
            self.message = Some(match procs::set_priority(pid, prio) {
                Ok(()) => (format!("Приоритет: {}", prio.title()), false),
                Err(e) => (format!("Приоритет не изменён: {e}"), true),
            });
        }
        if let Some((pid, stop)) = suspend {
            let r = if stop { procs::suspend(pid) } else { procs::resume(pid) };
            self.message = Some(match r {
                Ok(()) => (if stop { "Процесс приостановлен".into() } else { "Процесс возобновлён".into() }, false),
                Err(e) => (format!("Не удалось: {e}"), true),
            });
        }
    }

    fn kill_dialog(&mut self, ctx: &egui::Context) {
        let Some(k) = &self.kill else { return };
        let (mut go, mut cancel) = (false, false);
        let names: Vec<String> = k.pids.iter().filter_map(|pid| self.list.iter().find(|p| p.pid == *pid)).map(|p| format!("{} ({})", p.name, p.pid)).collect();
        egui::Modal::new(Id::new("kill-dialog")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new(if k.tree { "Завершить дерево процессов" } else { "Завершить процесс" }).size(19.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(&k.title).strong());
            if k.tree {
                ui.label(theme::dim(ui, format!("Будет завершено процессов: {}", names.len())));
                for n in names.iter().take(8) {
                    ui.label(theme::dim(ui, format!("   {n}")));
                }
                if names.len() > 8 {
                    ui.label(theme::dim(ui, format!("   …и ещё {}", names.len() - 8)));
                }
            }
            ui.add_space(4.0);
            ui.label("Несохранённые данные в программе будут потеряны.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::danger_button(ui, "Завершить").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    go = true;
                }
                if ui.button("Отмена").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
            });
        });
        if go {
            let k = self.kill.take().expect("kill");
            let (done, failed) = procs::terminate_many(&k.pids, &self.list, self.own_pid());
            self.message = Some(if failed.is_empty() {
                (format!("Завершено процессов: {done}"), false)
            } else {
                (format!("Завершено {done}, не удалось {}: {}", failed.len(), failed.iter().take(2).map(|(pid, e)| format!("{pid} — {e}")).collect::<Vec<_>>().join("; ")), true)
            });
            if let Some(l) = &self.live {
                // refresh right away instead of waiting for the next tick
                l.last_seen.store(now_ms(), Relaxed);
            }
        } else if cancel {
            self.kill = None;
        }
    }
}
