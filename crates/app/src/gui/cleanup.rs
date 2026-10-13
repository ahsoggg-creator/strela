//! "Очистка": the biggest files and folders below a path, with one-click fast delete / recycle.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;

use eframe::egui::{self, Id, RichText, Sense, Ui, pos2, vec2};
use strela_core::largest::{Big, Largest};
use strela_core::progress::Progress;
use strela_core::util;
use strela_core::winapi;

use super::actions;
use super::icons::Icons;
use super::theme::{self, pal};
use crate::commands::{self, Command};
use crate::state::Shared;

struct Run {
    progress: Arc<Progress>,
    rx: Receiver<Result<Largest, String>>,
}

pub struct CleanupTab {
    pub path: String,
    run: Option<Run>,
    result: Option<Largest>,
    marked: HashSet<PathBuf>,
    error: Option<String>,
    top: usize,
    pub start_now: bool,
}

impl CleanupTab {
    pub fn new() -> CleanupTab {
        CleanupTab { path: String::new(), run: None, result: None, marked: HashSet::new(), error: None, top: 100, start_now: false }
    }

    pub fn scan(&mut self, shared: &Arc<Shared>, ctx: &egui::Context) {
        let path = self.path.trim().trim_matches('"').to_string();
        if path.is_empty() {
            return;
        }
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (p2, sh, top, ctx) = (progress.clone(), shared.clone(), self.top, ctx.clone());
        std::thread::spawn(move || {
            // the index answers in milliseconds when it covers the path; otherwise walk the folder
            let r = match sh.backend.largest_from_index(&path, top) {
                Some(l) => Ok(l),
                None => strela_core::largest::from_walk(std::path::Path::new(&path), top, &p2).map_err(|e| e.to_string()),
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.run = Some(Run { progress, rx });
        self.result = None;
        self.error = None;
        self.marked.clear();
    }

    /// Removes deleted items from the shown lists.
    pub fn after_delete(&mut self, gone: &[PathBuf]) {
        if let Some(r) = &mut self.result {
            let is_gone = |p: &PathBuf| gone.iter().any(|g| p == g || p.starts_with(g));
            r.files.retain(|b| !is_gone(&b.path));
            r.folders.retain(|b| !is_gone(&b.path));
        }
        self.marked.retain(|p| !gone.iter().any(|g| p == g || p.starts_with(g)));
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>, icons: &mut Icons) {
        let p = pal(ui);
        if self.start_now {
            self.start_now = false;
            self.scan(shared, ui.ctx());
        }
        if let Some(run) = &self.run
            && let Ok(r) = run.rx.try_recv()
        {
            match r {
                Ok(l) => self.result = Some(l),
                Err(e) => self.error = Some(e),
            }
            self.run = None;
        }
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::heading(ui, "Самые большие файлы и папки");
            ui.label(theme::dim(ui, "Выберите диск или папку — Strela покажет, что занимает место, чтобы удалить это за пару кликов."));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.path).hint_text(r"C:\ или любая папка").desired_width(420.0).margin(vec2(10.0, 6.0)));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.scan(shared, ui.ctx());
                }
                if ui.button("Обзор…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_folder()
                {
                    self.path = f.to_string_lossy().into_owned();
                }
                ui.add_enabled_ui(self.run.is_none() && !self.path.trim().is_empty(), |ui| {
                    if theme::primary_button(ui, "Найти").clicked() {
                        self.scan(shared, ui.ctx());
                    }
                });
            });
            ui.horizontal_wrapped(|ui| {
                for d in winapi::drive_letters(false) {
                    if ui.small_button(format!("{d}:\\")).clicked() {
                        self.path = format!("{d}:\\");
                        self.scan(shared, ui.ctx());
                    }
                }
            });
            if let Some(run) = &self.run {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().color(p.accent));
                    ui.label(format!("Просмотрено файлов: {}", run.progress.items.load(std::sync::atomic::Ordering::Relaxed)));
                    if ui.button("Остановить").clicked() {
                        run.progress.cancel();
                    }
                });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            if let Some(e) = &self.error {
                ui.colored_label(p.err, e);
            }
        });
        ui.add_space(10.0);
        let Some(res) = &self.result else { return };
        let marked_size: u64 = res
            .files
            .iter()
            .chain(res.folders.iter())
            .filter(|b| self.marked.contains(&b.path) && !self.marked.iter().any(|m| m != &b.path && b.path.starts_with(m)))
            .map(|b| b.size)
            .sum();
        ui.label(theme::dim(ui, format!("Всего: {} файлов, {}", res.total_files, util::bytes(res.total_bytes))));
        let bar_h = 46.0;
        let avail = ui.available_height() - bar_h - 8.0;
        ui.columns(2, |cols| {
            self.list(&mut cols[0], shared, icons, "Файлы", true, avail);
            self.list(&mut cols[1], shared, icons, "Папки", false, avail);
        });
        ui.add_space(6.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(format!("Отмечено: {} ({})", self.marked.len(), util::bytes(marked_size)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!self.marked.is_empty(), |ui| {
                        if theme::danger_button(ui, "Удалить быстро…").clicked() {
                            self.delete(shared, "fast-delete");
                        }
                        if ui.add(egui::Button::new("В корзину…").min_size(vec2(0.0, 32.0))).clicked() {
                            self.delete(shared, "recycle");
                        }
                    });
                });
            });
        });
    }

    fn delete(&mut self, shared: &Arc<Shared>, verb: &str) {
        let paths: Vec<String> = self.marked.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        commands::execute(shared, Command { verb: verb.into(), paths, dest: None });
    }

    fn list(&mut self, ui: &mut Ui, shared: &Arc<Shared>, icons: &mut Icons, title: &str, files: bool, height: f32) {
        let p = pal(ui);
        let Some(res) = &self.result else { return };
        let items: &Vec<Big> = if files { &res.files } else { &res.folders };
        let max = items.first().map_or(1, |b| b.size.max(1));
        ui.label(RichText::new(format!("{title} ({})", items.len())).strong());
        let row_h = 42.0;
        let mut toggled: Vec<PathBuf> = Vec::new();
        egui::ScrollArea::vertical().id_salt(title).max_height(height.max(120.0)).auto_shrink([false, false]).show_rows(ui, row_h, items.len(), |ui, range| {
            for i in range {
                let b = &items[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                theme::share_bar(ui, rect, b.size as f32 / max as f32, p.accent);
                if resp.hovered() {
                    ui.painter().rect_filled(rect, 6.0, p.card_hover.gamma_multiply(0.5));
                }
                let mut on = self.marked.contains(&b.path);
                let cb_rect = egui::Rect::from_min_size(pos2(rect.left() + 6.0, rect.center().y - 10.0), vec2(20.0, 20.0));
                if ui.put(cb_rect, egui::Checkbox::without_text(&mut on)).changed() {
                    toggled.push(b.path.clone());
                }
                let name = b.path.file_name().map_or(b.path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
                if let Some(tex) = icons.get(ui.ctx(), &name, !files) {
                    ui.painter().image(tex.id(), egui::Rect::from_min_size(pos2(rect.left() + 34.0, rect.center().y - 11.0), vec2(22.0, 22.0)), egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), egui::Color32::WHITE);
                }
                let tx = rect.left() + 64.0;
                let painter = ui.painter_at(rect);
                painter.text(pos2(tx, rect.top() + 7.0), egui::Align2::LEFT_TOP, &name, egui::FontId::proportional(14.0), p.text);
                let parent = b.path.parent().map(winapi::display_path).unwrap_or_default();
                let clip = painter.with_clip_rect(egui::Rect::from_min_max(pos2(tx, rect.top()), pos2(rect.right() - 90.0, rect.bottom())));
                clip.text(pos2(tx, rect.top() + 24.0), egui::Align2::LEFT_TOP, parent, egui::FontId::proportional(11.5), p.dim);
                painter.text(pos2(rect.right() - 10.0, rect.center().y), egui::Align2::RIGHT_CENTER, util::bytes(b.size), egui::FontId::proportional(13.5), p.text);
                if resp.double_clicked() {
                    actions::reveal(&b.path.to_string_lossy());
                }
                let sh = shared.clone();
                let path = b.path.clone();
                resp.context_menu(|ui| {
                    if actions::path_menu(ui, &sh, std::slice::from_ref(&path)) {
                        ui.close();
                    }
                });
            }
        });
        for t in toggled {
            if !self.marked.remove(&t) {
                self.marked.insert(t);
            }
        }
        let _ = Id::NULL;
    }
}
