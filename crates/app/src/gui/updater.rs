//! The update dialog: release notes, one button, download progress.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use eframe::egui::{self, Id, Key, RichText};
use strela_core::util;

use super::theme::{self, pal};
use crate::state::Shared;
use crate::update::{self, Release, UpdateState};

pub struct UpdateDialog {
    rel: Release,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    installing: bool,
    error: Option<Arc<std::sync::Mutex<Option<String>>>>,
}

impl UpdateDialog {
    pub fn new(rel: Release) -> UpdateDialog {
        UpdateDialog { total: Arc::new(AtomicU64::new(rel.exe_size)), rel, done: Arc::new(AtomicU64::new(0)), cancel: Arc::new(AtomicBool::new(false)), installing: false, error: None }
    }

    fn start(&mut self, shared: &Arc<Shared>, ctx: &egui::Context) {
        self.installing = true;
        self.done.store(0, Relaxed);
        self.cancel.store(false, Relaxed);
        let err = Arc::new(std::sync::Mutex::new(None));
        self.error = Some(err.clone());
        *shared.update.lock().unwrap() = UpdateState::Installing;
        let (rel, done, total, cancel, shared, ctx) = (self.rel.clone(), self.done.clone(), self.total.clone(), self.cancel.clone(), shared.clone(), ctx.clone());
        std::thread::spawn(move || {
            let c2 = ctx.clone();
            let r = update::install(&rel, &|d, t| {
                done.store(d, Relaxed);
                total.store(t, Relaxed);
                c2.request_repaint();
            }, &cancel);
            match r {
                Ok(exe) => {
                    update::restart(&exe);
                    shared.quit();
                }
                Err(e) => {
                    *shared.update.lock().unwrap() = UpdateState::Available(rel);
                    *err.lock().unwrap() = Some(e);
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Returns true when the dialog should close.
    pub fn ui(&mut self, ctx: &egui::Context, shared: &Arc<Shared>) -> bool {
        let p = {
            let dark = ctx.global_style().visuals.dark_mode;
            let c = ctx.global_style().visuals.selection.stroke.color;
            theme::palette(dark, [c.r(), c.g(), c.b()])
        };
        let failed = self.error.as_ref().and_then(|e| e.lock().unwrap().clone());
        if failed.is_some() {
            self.installing = false;
        }
        let (mut go, mut close) = (false, false);
        egui::Modal::new(Id::new("update-dialog")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(RichText::new(format!("Доступна версия {}", self.rel.version)).size(19.0).strong());
            ui.label(theme::dim(ui, format!("Установлена: {}", update::CURRENT)));
            ui.add_space(6.0);
            if !self.rel.notes.trim().is_empty() {
                egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                    for line in self.rel.notes.lines().take(40) {
                        let l = line.trim_start_matches('#').trim().replace("**", "").replace('`', "");
                        if !l.is_empty() {
                            ui.label(l);
                        }
                    }
                });
            }
            ui.add_space(8.0);
            if self.installing {
                let (d, t) = (self.done.load(Relaxed), self.total.load(Relaxed));
                let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                theme::progress_bar(ui, Id::new("update-progress"), frac, true, p.accent, 10.0);
                ui.label(theme::dim(ui, if frac >= 1.0 { "Проверка и установка…".to_string() } else { format!("Загрузка: {} из {}", util::bytes(d), util::bytes(t)) }));
                if ui.button("Отмена").clicked() {
                    self.cancel.store(true, Relaxed);
                }
            } else {
                if let Some(e) = &failed {
                    ui.colored_label(p.err, e);
                    ui.add_space(4.0);
                }
                ui.label(theme::dim(ui, "Strela скачает Strela.exe из релиза на GitHub, сверит SHA-256 с SHA256SUMS.txt и перезапустится. Исполняемые файлы не подписаны."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, "Обновить и перезапустить").clicked() {
                        go = true;
                    }
                    if ui.button("Страница релиза").clicked() {
                        super::actions::open_path(&self.rel.page);
                    }
                    if ui.button("Позже").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            }
        });
        if go {
            self.start(shared, ctx);
        }
        let _ = pal;
        close
    }
}
