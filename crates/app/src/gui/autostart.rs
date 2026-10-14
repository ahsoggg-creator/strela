//! "Автозагрузка": everything that starts with Windows or at logon, in one list — Run keys, Startup folders, services,
//! scheduled tasks and the sneaky spots — with switches that agree with Task Manager.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Id, Key, RichText, Sense, Ui, vec2};
use strela_core::startup::{self, Entry, Group, Kind, StartType};

use super::actions;
use super::table::{self, Col, Sort, col};
use super::theme::{self, pal};
use crate::state::Shared;

const COLS: [Col; 5] = [col("", 54.0), col("Название", 0.0), col("Издатель", 170.0), col("Тип", 150.0), col("Команда", 0.0)];

fn id_of(e: &Entry) -> String {
    format!("{:?}|{}|{}|{}", e.group, e.origin, e.name, e.command)
}

fn explain(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5) {
        "отказано в доступе — запустите Strela от администратора".into()
    } else {
        e.to_string()
    }
}

pub struct StartupTab {
    entries: Vec<Entry>,
    loading: Option<Receiver<Vec<Entry>>>,
    loaded: bool,
    filter: String,
    sort: Sort,
    groups: [bool; 5],
    show_system: bool,
    manual_services: bool,
    only_issues: bool,
    selected: Option<String>,
    busy: Option<Receiver<Result<String, String>>>,
    message: Option<(String, bool)>,
    confirm_remove: Option<Entry>,
}

impl StartupTab {
    pub fn new() -> StartupTab {
        StartupTab {
            entries: Vec::new(),
            loading: None,
            loaded: false,
            filter: String::new(),
            sort: Sort { col: 3, desc: false },
            groups: [true; 5],
            show_system: false,
            manual_services: false,
            only_issues: false,
            selected: None,
            busy: None,
            message: None,
            confirm_remove: None,
        }
    }

    fn reload(&mut self, ctx: &egui::Context) {
        if self.loading.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(startup::list());
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    fn run(&mut self, ctx: &egui::Context, f: impl FnOnce() -> Result<String, String> + Send + 'static) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
        self.busy = Some(rx);
    }

    fn visible(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                let gi = Group::ALL.iter().position(|g| *g == e.group).unwrap_or(0);
                self.groups[gi]
                    && (self.show_system || !(e.system || (e.group == Group::Service && e.is_microsoft())))
                    && (self.manual_services || !matches!(&e.kind, Kind::Service { start: StartType::Manual | StartType::Disabled, .. }))
                    && (!self.only_issues || e.warning.is_some())
                    && table::matches(&self.filter, &[&e.name, &e.command, e.company.as_deref().unwrap_or(""), &e.origin])
            })
            .collect();
        let s = self.sort;
        v.sort_by(|&x, &y| {
            let (a, b) = (&self.entries[x], &self.entries[y]);
            let o = match s.col {
                0 => a.enabled.cmp(&b.enabled),
                2 => a.company.as_deref().unwrap_or("").to_lowercase().cmp(&b.company.as_deref().unwrap_or("").to_lowercase()),
                3 => a.group.cmp(&b.group).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                4 => a.command.to_lowercase().cmp(&b.command.to_lowercase()),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if s.desc { o.reverse() } else { o }
        });
        v
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>) {
        let p = pal(ui);
        if !self.loaded && self.loading.is_none() {
            self.reload(ui.ctx());
        }
        if let Some(rx) = &self.loading
            && let Ok(list) = rx.try_recv()
        {
            self.entries = list;
            self.loading = None;
            self.loaded = true;
        }
        if let Some(rx) = &self.busy
            && let Ok(r) = rx.try_recv()
        {
            self.message = Some(match r {
                Ok(m) => (m, false),
                Err(e) => (e, true),
            });
            self.busy = None;
            self.loaded = false;
        }
        self.remove_dialog(ui.ctx());
        super::actions::admin_banner(ui, shared, "startup", "Изменять машинные записи, службы и задачи Windows можно только с правами администратора.");
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::heading(ui, "Автозагрузка");
                if self.loading.is_some() || self.busy.is_some() {
                    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                }
            });
            let on = self.entries.iter().filter(|e| e.enabled && e.group != Group::Sneaky).count();
            let bad = self.entries.iter().filter(|e| e.warning.is_some()).count();
            ui.label(theme::dim(ui, format!("Всего записей: {} · включено: {on} · требуют внимания: {bad}", self.entries.len())));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                table::filter_box(ui, &mut self.filter, "Поиск по названию, команде, издателю", 300.0);
                if ui.button("Обновить").clicked() {
                    self.loaded = false;
                }
                ui.checkbox(&mut self.show_system, "системные (Microsoft)");
                ui.checkbox(&mut self.manual_services, "ручные и отключённые службы");
                ui.checkbox(&mut self.only_issues, "только с предупреждением");
            });
            ui.horizontal_wrapped(|ui| {
                for (i, g) in Group::ALL.iter().enumerate() {
                    let n = self.entries.iter().filter(|e| e.group == *g).count();
                    let hint = if *g == Group::Task && n == 0 && !strela_core::winapi::is_elevated() { " · нужны права администратора" } else { "" };
                    ui.toggle_value(&mut self.groups[i], format!("{} · {n}{hint}", g.title()));
                }
            });
            if let Some((m, err)) = &self.message {
                ui.colored_label(if *err { p.err } else { p.ok }, m);
            }
        });
        ui.add_space(8.0);
        let idx = self.visible();
        table::header(ui, &COLS, &mut self.sort);
        let details_h = 132.0;
        let list_h = (ui.available_height() - details_h - 8.0).max(120.0);
        let row_h = 38.0;
        let mut toggle: Option<usize> = None;
        let mut select: Option<String> = None;
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show_rows(ui, row_h, idx.len(), |ui, range| {
            for r in range {
                let i = idx[r];
                let e = &self.entries[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                let sel = self.selected.as_deref() == Some(id_of(e).as_str());
                table::row_bg(ui, rect, resp.hovered(), sel);
                let xs = table::layout(rect.width(), &COLS);
                let sw_rect = egui::Rect::from_min_size(egui::pos2(rect.left() + xs[0].0, rect.top()), vec2(xs[0].1, rect.height()));
                let can = e.group != Group::Sneaky;
                if table::switch(ui, sw_rect, e.enabled, can) {
                    toggle = Some(i);
                }
                let dim_if_off = if e.enabled { p.text } else { p.dim };
                table::cell(ui, rect, &COLS[1], xs[1], &e.name, dim_if_off, 14.0);
                table::cell(ui, rect, &COLS[2], xs[2], e.company.as_deref().unwrap_or(""), p.dim, 12.5);
                table::cell(ui, rect, &COLS[3], xs[3], e.group.title(), p.dim, 12.5);
                let (cmd_color, cmd) = match &e.warning {
                    Some(w) => (p.warn, format!("⚠ {w}")),
                    None => (p.dim, e.command.clone()),
                };
                table::cell(ui, rect, &COLS[4], xs[4], &cmd, cmd_color, 12.0);
                if resp.clicked() {
                    select = Some(id_of(e));
                }
                resp.on_hover_text(&e.command);
            }
        });
        if let Some(s) = select {
            self.selected = Some(s);
        }
        if let Some(i) = toggle {
            let e = self.entries[i].clone();
            let on = !e.enabled;
            let name = e.name.clone();
            self.run(ui.ctx(), move || startup::set_enabled(&e, on).map(|_| format!("«{name}»: {}", if on { "включено" } else { "отключено" })).map_err(|er| format!("«{name}»: {}", explain(&er))));
        }
        ui.add_space(6.0);
        let sel = self.selected.as_ref().and_then(|s| self.entries.iter().find(|e| id_of(e) == *s)).cloned();
        self.details(ui, sel);
    }

    fn details(&mut self, ui: &mut Ui, sel: Option<Entry>) {
        let p = pal(ui);
        let mut remove: Option<Entry> = None;
        let mut action: Option<Box<dyn FnOnce() -> Result<String, String> + Send>> = None;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let Some(e) = &sel else {
                ui.label(theme::dim(ui, "Выберите запись, чтобы увидеть подробности. Переключатель слева включает и отключает автозапуск — так же, как диспетчер задач."));
                ui.set_min_height(86.0);
                return;
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(&e.name).strong().size(15.0));
                if let Some(c) = &e.company {
                    ui.label(theme::dim(ui, format!("· {c}")));
                }
            });
            if let Some(d) = &e.description {
                ui.add(egui::Label::new(theme::dim(ui, d.as_str())).truncate());
            }
            ui.add(egui::Label::new(RichText::new(&e.command).size(12.5)).truncate());
            ui.add(egui::Label::new(theme::dim(ui, format!("Источник: {}", e.origin))).truncate());
            if let Some(w) = &e.warning {
                ui.colored_label(p.warn, format!("⚠ {w}"));
            }
            ui.horizontal_wrapped(|ui| {
                if let Some(path) = &e.path
                    && ui.button("Показать файл").clicked()
                {
                    actions::reveal(&path.to_string_lossy());
                }
                if ui.button("Копировать команду").clicked() {
                    ui.ctx().copy_text(e.command.clone());
                }
                match &e.kind {
                    Kind::RunValue { key, .. } | Kind::Sneaky { key, .. } => {
                        if ui.button("Открыть в regedit").clicked() {
                            startup::open_in_regedit(key);
                        }
                    }
                    Kind::Service { name, start, running, .. } => {
                        let mut cur = *start;
                        egui::ComboBox::from_id_salt("svc-start").selected_text(cur.title()).show_ui(ui, |ui| {
                            for t in [StartType::Auto, StartType::AutoDelayed, StartType::Manual, StartType::Disabled] {
                                ui.selectable_value(&mut cur, t, t.title());
                            }
                        });
                        if cur != *start && !matches!(start, StartType::Other(_)) {
                            let n = name.clone();
                            action = Some(Box::new(move || startup::set_service_start(&n, cur).map(|_| format!("Служба {n}: «{}»", cur.title())).map_err(|er| explain(&er))));
                        }
                        let n = name.clone();
                        if *running {
                            if ui.button("Остановить").clicked() {
                                action = Some(Box::new(move || startup::stop_service(&n).map(|_| format!("Служба {n} остановлена")).map_err(|er| explain(&er))));
                            }
                        } else if ui.button("Запустить").clicked() {
                            action = Some(Box::new(move || startup::start_service(&n).map(|_| format!("Служба {n} запущена")).map_err(|er| explain(&er))));
                        }
                    }
                    _ => {}
                }
                if e.group != Group::Sneaky && ui.add(egui::Button::new(RichText::new("Удалить из автозагрузки…").color(p.err))).clicked() {
                    remove = Some(e.clone());
                }
            });
        });
        if let Some(a) = action {
            self.run(ui.ctx(), a);
        }
        if remove.is_some() {
            self.confirm_remove = remove;
        }
    }

    fn remove_dialog(&mut self, ctx: &egui::Context) {
        let Some(e) = self.confirm_remove.clone() else { return };
        let mut close = false;
        let mut go = false;
        egui::Modal::new(Id::new("startup-remove")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(RichText::new("Удалить из автозагрузки").size(19.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(&e.name).strong());
            ui.add(egui::Label::new(theme::dim(ui, e.command.as_str())).truncate());
            ui.add_space(6.0);
            let what = match e.group {
                Group::Service => "Служба будет остановлена и удалена из системы.",
                Group::Task => "Задача будет удалена из планировщика.",
                Group::Folder => "Ярлык будет удалён из папки автозагрузки.",
                _ => "Значение будет удалено из реестра.",
            };
            ui.label(what);
            ui.label(theme::dim(ui, "Резервная копия записи сохранится в папке резервных копий Strela. Если нужно лишь временно отключить — используйте переключатель в списке."));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::danger_button(ui, "Удалить").clicked() {
                    go = true;
                }
                if ui.button("Отмена").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
            });
        });
        if go {
            close = true;
            let dir = strela_core::util::data_dir().join("backups").join(format!("autostart-{}", strela_core::util::date_utc(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)).replace([':', ' '], "-")));
            let name = e.name.clone();
            self.run(ctx, move || startup::remove(&e, &dir).map(|_| format!("«{name}» удалено из автозагрузки")).map_err(|er| format!("«{name}»: {}", explain(&er))));
        }
        if close {
            self.confirm_remove = None;
        }
    }
}
