//! "Передача": start a copy/move, watch the queue (progress, live speed chart, ETA), pause / resume / cancel.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Color32, Id, RichText, Ui, vec2};
use strela_core::transfer::queue::{JobInfo, JobState};
use strela_core::transfer::{HashAlgo, Op, Options, Overwrite};
use strela_core::util;
use strela_core::winapi;

use super::theme::{self, pal};
use crate::state::Shared;

pub struct TransferTab {
    pub sources: Vec<PathBuf>,
    pub dest: String,
    pub op: Op,
    pub verify: bool,
    pub algo: HashAlgo,
    pub overwrite: Overwrite,
    pub hardlink: bool,
    pub hide_finished: bool,
    pub message: Option<(String, bool)>,
}

impl TransferTab {
    pub fn new(shared: &Shared) -> TransferTab {
        let s = shared.settings();
        TransferTab {
            sources: Vec::new(),
            dest: s.last_dest.clone(),
            op: Op::Copy,
            verify: s.verify_copies,
            algo: s.hash,
            overwrite: Overwrite::Replace,
            hardlink: false,
            hide_finished: false,
            message: None,
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>) {
        let p = pal(ui);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(1100.0));
            self.new_card(ui, shared);
            ui.add_space(12.0);
            let jobs = shared.queue.list();
            ui.horizontal(|ui| {
                theme::heading(ui, "Очередь");
                ui.add_space(8.0);
                ui.label(theme::dim(ui, format!("{} задач", jobs.len())));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Очистить завершённые").clicked() {
                        shared.queue.clear_finished();
                    }
                    ui.checkbox(&mut self.hide_finished, "Скрывать завершённые");
                });
            });
            ui.add_space(4.0);
            let mut any = false;
            for job in jobs.iter().rev() {
                if self.hide_finished && job.state.is_finished() {
                    continue;
                }
                any = true;
                job_card(ui, shared, job);
                ui.add_space(8.0);
            }
            if !any {
                theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        ui.add_space(14.0);
                        ui.label(RichText::new("Задач пока нет").color(p.dim));
                        ui.label(theme::dim(ui, "Выберите файлы выше или используйте «Strela ▸» в меню Проводника"));
                        ui.add_space(14.0);
                    });
                });
            }
        });
        let any_running = shared.queue.list().iter().any(|j| matches!(j.state, JobState::Running | JobState::Queued));
        if any_running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn new_card(&mut self, ui: &mut Ui, shared: &Arc<Shared>) {
        let p = pal(ui);
        let staged = shared.staging.lock().unwrap().paths.len();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::heading(ui, "Новая передача");
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("+ Файлы…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_files()
                {
                    self.sources.extend(f);
                }
                if ui.button("+ Папка…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_folder()
                {
                    self.sources.push(f);
                }
                if staged > 0 && ui.button(format!("Из буфера Strela ({staged})")).clicked() {
                    let mut st = shared.staging.lock().unwrap();
                    self.op = if st.cut { Op::Move } else { Op::Copy };
                    self.sources.append(&mut st.paths);
                }
                if !self.sources.is_empty() && ui.button("Очистить список").clicked() {
                    self.sources.clear();
                }
            });
            if !self.sources.is_empty() {
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(110.0).id_salt("src-list").show(ui, |ui| {
                    let mut remove = None;
                    for (i, s) in self.sources.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                            ui.label(RichText::new(winapi::display_path(s)).size(13.0));
                        });
                    }
                    if let Some(i) = remove {
                        self.sources.remove(i);
                    }
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Куда:");
                ui.add(egui::TextEdit::singleline(&mut self.dest).hint_text(r"D:\Папка").desired_width(360.0).margin(vec2(10.0, 6.0)));
                if ui.button("Обзор…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_folder()
                {
                    self.dest = f.to_string_lossy().into_owned();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::dim(ui, "Диски:"));
                for d in winapi::drive_letters(true) {
                    let root = format!("{d}:\\");
                    let space = winapi::disk_space(std::path::Path::new(&root));
                    let label = match space {
                        Some((total, free)) => format!("{d}:  {} своб. из {}", util::bytes(free), util::bytes(total)),
                        None => format!("{d}:"),
                    };
                    if ui.small_button(label).clicked() {
                        self.dest = root;
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.op, Op::Copy, "Копирование");
                ui.selectable_value(&mut self.op, Op::Move, "Перемещение");
                ui.add_space(14.0);
                ui.checkbox(&mut self.verify, "Проверять целостность");
                if self.verify {
                    egui::ComboBox::from_id_salt("algo")
                        .selected_text(match self.algo {
                            HashAlgo::Blake3 => "BLAKE3",
                            HashAlgo::Xxh3 => "xxh3 (быстрее)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.algo, HashAlgo::Blake3, "BLAKE3");
                            ui.selectable_value(&mut self.algo, HashAlgo::Xxh3, "xxh3 (быстрее)");
                        });
                }
                egui::ComboBox::from_id_salt("overwrite")
                    .selected_text(match self.overwrite {
                        Overwrite::Replace => "Заменять существующие",
                        Overwrite::Skip => "Пропускать существующие",
                        Overwrite::Newer => "Заменять более старые",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.overwrite, Overwrite::Replace, "Заменять существующие");
                        ui.selectable_value(&mut self.overwrite, Overwrite::Skip, "Пропускать существующие");
                        ui.selectable_value(&mut self.overwrite, Overwrite::Newer, "Заменять более старые");
                    });
                ui.checkbox(&mut self.hardlink, "Жёсткие ссылки (в пределах тома)");
            });
            if self.op == Op::Move {
                ui.label(theme::dim(ui, "В пределах одного диска перемещение — мгновенное переименование; между дисками — копирование, проверка, затем удаление источника."));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready = !self.sources.is_empty() && !self.dest.trim().is_empty();
                ui.add_enabled_ui(ready, |ui| {
                    if theme::primary_button(ui, "Начать").clicked() {
                        self.start(shared);
                    }
                });
                if let Some((m, err)) = &self.message {
                    ui.label(RichText::new(m).color(if *err { p.err } else { p.ok }));
                }
            });
        });
    }

    fn start(&mut self, shared: &Arc<Shared>) {
        let dest = PathBuf::from(self.dest.trim().trim_matches('"'));
        let options = Options {
            verify: self.verify.then_some(self.algo),
            overwrite: self.overwrite,
            hardlink: self.hardlink,
            ..Options::default()
        };
        let spec = strela_core::transfer::JobSpec {
            op: self.op,
            sources: std::mem::take(&mut self.sources),
            dest_dir: dest.clone(),
            options,
            typed_confirm: None,
        };
        shared.queue.add(spec);
        {
            let mut s = shared.settings.write().unwrap();
            s.last_dest = self.dest.clone();
            s.save();
        }
        self.message = Some(("Добавлено в очередь".into(), false));
    }
}

fn state_chip(ui: &mut Ui, s: JobState) {
    let p = pal(ui);
    let (text, color) = match s {
        JobState::Queued => ("В очереди", p.dim),
        JobState::Running => ("Идёт", p.accent),
        JobState::Paused => ("Пауза", p.warn),
        JobState::Done => ("Готово", p.ok),
        JobState::DoneWithErrors => ("С ошибками", p.warn),
        JobState::Failed => ("Ошибка", p.err),
        JobState::Cancelled => ("Отменено", p.dim),
        JobState::Interrupted => ("Прервано", p.warn),
    };
    theme::chip(ui, text, color);
}

fn job_card(ui: &mut Ui, shared: &Arc<Shared>, job: &JobInfo) {
    let p = pal(ui);
    let appear = theme::appear(ui, Id::new(("job-in", job.id)));
    let id = job.id;
    ui.scope(|ui| {
        ui.set_opacity(appear);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let verb = if job.spec.op == Op::Move { "Перемещение" } else { "Копирование" };
                ui.label(RichText::new(format!("{verb} · {} элем.", job.spec.sources.len())).strong().size(15.0));
                state_chip(ui, job.state);
                ui.label(theme::dim(ui, format!("→ {}", winapi::display_path(&job.spec.dest_dir))));
            });
            let pr = &job.progress;
            let frac = if pr.bytes_total > 0 { pr.bytes_done as f32 / pr.bytes_total as f32 } else if job.state == JobState::Done { 1.0 } else { 0.0 };
            let active = job.state == JobState::Running;
            let color = match job.state {
                JobState::Done => p.ok,
                JobState::DoneWithErrors | JobState::Paused | JobState::Interrupted => p.warn,
                JobState::Failed => p.err,
                _ => p.accent,
            };
            if !matches!(job.state, JobState::Cancelled) {
                ui.add_space(4.0);
                theme::progress_bar(ui, Id::new(("pb", id)), frac, active, color, 12.0);
            }
            match job.state {
                JobState::Running | JobState::Paused | JobState::Queued => {
                    let eta = pr.eta.map_or("—".to_string(), util::clock);
                    ui.label(
                        RichText::new(format!(
                            "{:.0}%  ·  {} из {}  ·  {}  ·  осталось {}  ·  файлов {}/{}",
                            frac * 100.0,
                            util::bytes(pr.bytes_done),
                            util::bytes(pr.bytes_total),
                            util::speed(pr.speed),
                            eta,
                            pr.files_done,
                            pr.files_total
                        ))
                        .size(13.0),
                    );
                    if !pr.current.is_empty() {
                        ui.add(egui::Label::new(theme::dim(ui, &pr.current)).truncate());
                    }
                    if job.speed_history.len() > 3 {
                        ui.add_space(4.0);
                        theme::speed_chart(ui, &job.speed_history, 90.0);
                    }
                }
                JobState::Done | JobState::DoneWithErrors => {
                    if let Some(r) = &job.report {
                        let mut line = format!("{} {}, {} за {}", r.files_copied, util::plural(r.files_copied, "файл", "файла", "файлов"), util::bytes(r.bytes), util::clock(r.elapsed));
                        if r.files_skipped > 0 {
                            line += &format!(" · пропущено {}", r.files_skipped);
                        }
                        if r.instant_moves > 0 {
                            line += &format!(" · перемещено переименованием: {}", r.instant_moves);
                        }
                        if r.cloned > 0 {
                            line += &format!(" · клонировано блоков: {}", r.cloned);
                        }
                        ui.label(RichText::new(line).size(13.0));
                        for n in &r.notes {
                            ui.label(theme::dim(ui, n));
                        }
                        if !r.errors.is_empty() {
                            egui::CollapsingHeader::new(RichText::new(format!("Ошибки: {}", r.errors.len())).color(p.warn)).id_salt(("err", id)).show(ui, |ui| {
                                for e in r.errors.iter().take(50) {
                                    ui.label(theme::dim(ui, format!("{} — {}", winapi::display_path(&e.path), e.message)));
                                }
                            });
                        }
                    }
                }
                JobState::Failed => {
                    ui.label(RichText::new(job.error.clone().unwrap_or_default()).color(p.err));
                }
                JobState::Interrupted => {
                    ui.label(theme::dim(ui, "Программа была закрыта во время передачи. Частично скопированные большие файлы будут продолжены с места остановки."));
                }
                JobState::Cancelled => {}
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| match job.state {
                JobState::Running => {
                    if ui.button("⏸ Пауза").clicked() {
                        shared.queue.pause(id);
                    }
                    if ui.button("✕ Отмена").clicked() {
                        shared.queue.cancel(id);
                    }
                }
                JobState::Paused => {
                    if ui.button("▶ Продолжить").clicked() {
                        shared.queue.resume(id);
                    }
                    if ui.button("✕ Отмена").clicked() {
                        shared.queue.cancel(id);
                    }
                }
                JobState::Queued => {
                    if ui.button("✕ Отмена").clicked() {
                        shared.queue.cancel(id);
                    }
                }
                JobState::Interrupted => {
                    if ui.button("↻ Продолжить").clicked() {
                        shared.queue.resume(id);
                    }
                    if ui.button("Убрать").clicked() {
                        shared.queue.remove(id);
                    }
                }
                _ => {
                    if ui.button("Убрать").clicked() {
                        shared.queue.remove(id);
                    }
                    if ui.button("Открыть папку").clicked() {
                        super::actions::open_path(&winapi::display_path(&job.spec.dest_dir));
                    }
                }
            });
        });
    });
    let _ = Color32::WHITE;
}
