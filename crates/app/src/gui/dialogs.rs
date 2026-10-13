//! Modal dialogs: delete confirmation (with totals — and the typed folder name for protected places) and the
//! "move to drive" picker.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;

use eframe::egui::{self, Id, Key, RichText, vec2};
use strela_core::delete::{self, DeleteReport, Preview};
use strela_core::guard::Guard;
use strela_core::progress::Progress;
use strela_core::transfer::Op;
use strela_core::util;
use strela_core::winapi;

use super::theme::{self, pal};
use crate::state::Shared;

pub enum Outcome {
    Open,
    Closed,
    /// Items that are gone now (so lists can drop them).
    Deleted(Vec<PathBuf>),
}

enum Stage {
    Loading { rx: Receiver<Result<Preview, String>>, progress: Arc<Progress> },
    Ready(Preview),
    Deleting { rx: Receiver<Result<DeleteReport, String>>, started: std::time::Instant },
    Finished { text: String, ok: bool, removed: Vec<PathBuf> },
}

pub struct DeleteDialog {
    pub paths: Vec<PathBuf>,
    pub recycle: bool,
    stage: Stage,
    typed: String,
    error: Option<String>,
}

impl DeleteDialog {
    pub fn new(paths: Vec<PathBuf>, recycle: bool, ctx: &egui::Context) -> DeleteDialog {
        let (tx, rx) = channel();
        let progress = Arc::new(Progress::default());
        let (p2, ps, ctx) = (progress.clone(), paths.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = delete::preview(&ps, &Guard::system(), &p2).map_err(|e| e.to_string());
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        DeleteDialog { paths, recycle, stage: Stage::Loading { rx, progress }, typed: String::new(), error: None }
    }

    pub fn ui(&mut self, ctx: &egui::Context, shared: &Arc<Shared>) -> Outcome {
        let mut outcome = Outcome::Open;
        // stage transitions
        if let Stage::Loading { rx, .. } = &self.stage
            && let Ok(r) = rx.try_recv()
        {
            self.stage = match r {
                Ok(pv) => Stage::Ready(pv),
                Err(e) => Stage::Finished { text: e, ok: false, removed: Vec::new() },
            };
        }
        if let Stage::Deleting { rx, started } = &self.stage
            && let Ok(r) = rx.try_recv()
        {
            let _ = started;
            self.stage = match r {
                Ok(rep) if rep.failed.is_empty() => Stage::Finished {
                    text: format!("Удалено за {:.2} с: {} файлов, {}", rep.elapsed.as_secs_f64(), rep.files, util::bytes(rep.bytes)),
                    ok: true,
                    removed: self.paths.clone(),
                },
                Ok(rep) => {
                    let gone: Vec<PathBuf> = self.paths.iter().filter(|p| std::fs::symlink_metadata(p).is_err()).cloned().collect();
                    Stage::Finished {
                        text: format!(
                            "Удалено {} файлов, но {} элементов не удалось: {}",
                            rep.files,
                            rep.failed.len(),
                            rep.failed.iter().take(3).map(|(p, m)| format!("{} ({m})", winapi::display_path(p))).collect::<Vec<_>>().join("; ")
                        ),
                        ok: false,
                        removed: gone,
                    }
                }
                Err(e) => Stage::Finished { text: e, ok: false, removed: Vec::new() },
            };
        }
        let p = {
            let dark = ctx.global_style().visuals.dark_mode;
            let c = ctx.global_style().visuals.selection.stroke.color;
            theme::palette(dark, [c.r(), c.g(), c.b()])
        };
        let title = if self.recycle { "Переместить в корзину" } else { "Удалить безвозвратно" };
        let resp = egui::Modal::new(Id::new("delete-dialog")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(RichText::new(title).size(19.0).strong());
            ui.add_space(6.0);
            for path in self.paths.iter().take(6) {
                ui.add(egui::Label::new(RichText::new(winapi::display_path(path)).size(13.0)).truncate());
            }
            if self.paths.len() > 6 {
                ui.label(theme::dim(ui, format!("…и ещё {}", self.paths.len() - 6)));
            }
            ui.add_space(8.0);
            let mut confirm = false;
            let mut cancel = false;
            let mut close = false;
            match &mut self.stage {
                Stage::Loading { progress, .. } => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().color(p.accent));
                        ui.label("Подсчёт файлов и размера…");
                    });
                    if ui.button("Отмена").clicked() {
                        progress.cancel();
                        cancel = true;
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
                Stage::Ready(pv) => {
                    theme::card(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            for (n, label) in [(pv.files, "файлов"), (pv.dirs, "папок")] {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(n.to_string()).size(24.0).strong());
                                    ui.label(theme::dim(ui, label));
                                });
                                ui.add_space(24.0);
                            }
                            ui.vertical(|ui| {
                                ui.label(RichText::new(util::bytes(pv.bytes)).size(24.0).strong());
                                ui.label(theme::dim(ui, "общий размер"));
                            });
                        });
                        if pv.links > 0 {
                            ui.label(theme::dim(ui, format!("Ссылок: {} — удаляются сами ссылки, не то, на что они указывают", pv.links)));
                        }
                    });
                    if let Some(prot) = pv.needs_typed_confirmation().cloned() {
                        ui.add_space(8.0);
                        ui.colored_label(p.err, RichText::new(format!("Защищённое место: {}", prot.reason)).strong());
                        ui.label(format!("Это может сломать систему или потерять данные. Чтобы продолжить, введите «{}»:", prot.required_name));
                        ui.add(egui::TextEdit::singleline(&mut self.typed).desired_width(240.0).margin(vec2(10.0, 6.0)));
                    } else if !self.recycle {
                        ui.add_space(6.0);
                        ui.label(theme::dim(ui, "Безвозвратно: файлы не попадут в корзину."));
                    }
                    if let Some(e) = &self.error {
                        ui.colored_label(p.err, e);
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let go = if self.recycle { ui.add(egui::Button::new(RichText::new("В корзину").strong()).min_size(vec2(0.0, 32.0))) } else { theme::danger_button(ui, "Удалить") };
                        if go.clicked() || (pv.needs_typed_confirmation().is_none() && ctx.input(|i| i.key_pressed(Key::Enter))) {
                            confirm = true;
                        }
                        if ui.button("Отмена").clicked() {
                            cancel = true;
                        }
                    });
                }
                Stage::Deleting { started, .. } => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().color(p.accent));
                        ui.label(format!("Удаление… {:.1} с", started.elapsed().as_secs_f64()));
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
                Stage::Finished { text, ok, removed } => {
                    let color = if *ok { p.ok } else { p.err };
                    ui.colored_label(color, RichText::new(text.as_str()).size(14.5));
                    ui.add_space(8.0);
                    if ui.button("Закрыть").clicked() || ctx.input(|i| i.key_pressed(Key::Escape) || i.key_pressed(Key::Enter)) {
                        close = true;
                        outcome = Outcome::Deleted(std::mem::take(removed));
                    }
                }
            }
            if cancel {
                outcome = Outcome::Closed;
            }
            if close {
                // Outcome::Deleted already set
            }
            confirm
        });
        if resp.inner {
            self.confirm(ctx, shared);
        }
        if matches!(self.stage, Stage::Ready(_) | Stage::Loading { .. }) && ctx.input(|i| i.key_pressed(Key::Escape)) {
            outcome = Outcome::Closed;
        }
        outcome
    }

    fn confirm(&mut self, ctx: &egui::Context, _shared: &Arc<Shared>) {
        let Stage::Ready(pv) = std::mem::replace(&mut self.stage, Stage::Finished { text: String::new(), ok: false, removed: vec![] }) else { return };
        match pv.clone().confirm(Some(&self.typed)) {
            Err(e) => {
                self.error = Some(e.to_string());
                self.stage = Stage::Ready(pv);
            }
            Ok(confirmed) => {
                let (tx, rx) = channel();
                let (recycle, ctx) = (self.recycle, ctx.clone());
                std::thread::spawn(move || {
                    let r = if recycle {
                        delete::delete_to_recycle_bin(&confirmed)
                    } else {
                        delete::delete_permanently(&confirmed, &Progress::default())
                    };
                    let _ = tx.send(r.map_err(|e| e.to_string()));
                    ctx.request_repaint();
                });
                self.stage = Stage::Deleting { rx, started: std::time::Instant::now() };
            }
        }
    }
}

pub struct PickDialog {
    pub paths: Vec<PathBuf>,
    pub op: Op,
    dest: String,
}

impl PickDialog {
    pub fn new(paths: Vec<PathBuf>, op: Op, last: &str) -> PickDialog {
        PickDialog { paths, op, dest: last.to_string() }
    }

    /// `Some(true)` = started, `Some(false)` = cancelled.
    pub fn ui(&mut self, ctx: &egui::Context, shared: &Arc<Shared>) -> Option<bool> {
        let mut result = None;
        let verb = if self.op == Op::Move { "Переместить" } else { "Копировать" };
        egui::Modal::new(Id::new("pick-dialog")).show(ctx, |ui| {
            ui.set_width(560.0);
            let p = pal(ui);
            ui.label(RichText::new(format!("{verb} на диск")).size(19.0).strong());
            ui.label(theme::dim(ui, format!("Элементов: {}  ·  {}", self.paths.len(), self.paths.first().map(|p| winapi::display_path(p)).unwrap_or_default())));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for d in winapi::drive_letters(true) {
                    let root = format!("{d}:\\");
                    let selected = self.dest.to_lowercase().starts_with(&root.to_lowercase());
                    let same = self.paths.first().is_some_and(|p| winapi::same_volume(p, std::path::Path::new(&root)));
                    ui.vertical(|ui| {
                        let (rect, resp) = ui.allocate_exact_size(vec2(118.0, 74.0), egui::Sense::click());
                        let fill = if selected { p.accent.gamma_multiply(0.22) } else if resp.hovered() { p.card_hover } else { p.card };
                        ui.painter().rect_filled(rect, 10.0, fill);
                        ui.painter().rect_stroke(rect, 10.0, egui::Stroke::new(1.0, if selected { p.accent } else { p.border }), egui::StrokeKind::Inside);
                        ui.painter().text(rect.left_top() + vec2(10.0, 8.0), egui::Align2::LEFT_TOP, format!("{d}:"), egui::FontId::proportional(18.0), p.text);
                        if let Some((total, free)) = winapi::disk_space(std::path::Path::new(&root)) {
                            let frac = 1.0 - free as f32 / total as f32;
                            let bar = egui::Rect::from_min_size(rect.left_top() + vec2(10.0, 38.0), vec2(98.0, 6.0));
                            ui.painter().rect_filled(bar, 3.0, p.panel);
                            let mut f = bar;
                            f.set_width(bar.width() * frac);
                            ui.painter().rect_filled(f, 3.0, if frac > 0.9 { p.err } else { p.accent });
                            ui.painter().text(rect.left_top() + vec2(10.0, 50.0), egui::Align2::LEFT_TOP, format!("{} свободно", util::bytes(free)), egui::FontId::proportional(11.5), p.dim);
                        }
                        if same && self.op == Op::Move {
                            ui.painter().text(rect.right_top() + vec2(-8.0, 10.0), egui::Align2::RIGHT_TOP, "мгновенно", egui::FontId::proportional(10.5), p.ok);
                        }
                        if resp.clicked() {
                            self.dest = root;
                        }
                    });
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Папка:");
                ui.add(egui::TextEdit::singleline(&mut self.dest).hint_text(r"D:\Папка").desired_width(340.0).margin(vec2(10.0, 6.0)));
                if ui.button("Обзор…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_folder()
                {
                    self.dest = f.to_string_lossy().into_owned();
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.dest.trim().is_empty(), |ui| {
                    if theme::primary_button(ui, verb).clicked() || (ctx.input(|i| i.key_pressed(Key::Enter)) && !self.dest.trim().is_empty()) {
                        shared.start_transfer(self.paths.clone(), PathBuf::from(self.dest.trim().trim_matches('"')), self.op);
                        {
                            let mut s = shared.settings.write().unwrap();
                            s.last_dest = self.dest.clone();
                            s.save();
                        }
                        result = Some(true);
                    }
                });
                if ui.button("Отмена").clicked() || ctx.input(|i| i.key_pressed(Key::Escape)) {
                    result = Some(false);
                }
            });
        });
        result
    }
}
