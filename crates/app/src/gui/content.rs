//! "Содержимое": search inside files (ripgrep engine), grouped by file, with highlighted hits; Enter opens the file at
//! the line.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Key, Modifiers, RichText, Sense, Ui, pos2, text::LayoutJob, vec2};
use strela_core::grep::{self, FileHits, GrepOptions, GrepStats};
use strela_core::progress::Progress;
use strela_core::util;
use strela_core::winapi;

use super::actions;
use super::icons::Icons;
use super::theme::{self, pal};
use crate::state::Shared;

enum Msg {
    Candidates(usize),
    Hits(FileHits),
    Done(Result<GrepStats, String>, Duration),
}

struct Run {
    progress: Arc<Progress>,
    rx: Receiver<Msg>,
    candidates: usize,
    started: Instant,
}

pub struct ContentTab {
    pub pattern: String,
    pub root: String,
    pub filter: String,
    regex: bool,
    case_sensitive: bool,
    run: Option<Run>,
    groups: Vec<FileHits>,
    /// `(group, Some(match))` or `(group, None)` for the file header.
    flat: Vec<(usize, Option<usize>)>,
    selected: usize,
    status: String,
    error: Option<String>,
    pub start_now: bool,
}

impl ContentTab {
    pub fn new() -> ContentTab {
        ContentTab {
            pattern: String::new(),
            root: String::new(),
            filter: String::new(),
            regex: false,
            case_sensitive: false,
            run: None,
            groups: Vec::new(),
            flat: Vec::new(),
            selected: 0,
            status: String::new(),
            error: None,
            start_now: false,
        }
    }

    fn start(&mut self, shared: &Arc<Shared>, ctx: &egui::Context) {
        if self.pattern.is_empty() || self.run.is_some() {
            return;
        }
        let progress = Arc::new(Progress::default());
        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
        let opts = GrepOptions {
            pattern: self.pattern.clone(),
            regex: self.regex,
            case_insensitive: !self.case_sensitive,
            max_matches_per_file: 200,
            max_filesize: Some(256 << 20),
        };
        let (root, filter) = (self.root.trim().trim_matches('"').to_string(), self.filter.clone());
        let (sh, p2, ctx) = (shared.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let files = candidates(&sh, &root, &filter, &p2);
            let files = match files {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Msg::Done(Err(e), t0.elapsed()));
                    ctx.request_repaint();
                    return;
                }
            };
            let _ = tx.send(Msg::Candidates(files.len()));
            let tx2 = tx.clone();
            let ctx2 = ctx.clone();
            let r = grep::grep(&files, &opts, &p2, &move |h| {
                let _ = tx2.send(Msg::Hits(h));
                ctx2.request_repaint();
            });
            let _ = tx.send(Msg::Done(r.map_err(|e| e.to_string()), t0.elapsed()));
            ctx.request_repaint();
        });
        self.run = Some(Run { progress, rx, candidates: 0, started: Instant::now() });
        self.groups.clear();
        self.flat.clear();
        self.selected = 0;
        self.status.clear();
        self.error = None;
    }

    fn poll(&mut self) {
        let Some(run) = &mut self.run else { return };
        let mut done = false;
        let mut dirty = false;
        while let Ok(m) = run.rx.try_recv() {
            match m {
                Msg::Candidates(n) => run.candidates = n,
                Msg::Hits(h) => {
                    self.groups.push(h);
                    dirty = true;
                }
                Msg::Done(r, took) => {
                    match r {
                        Ok(s) => {
                            self.status = format!(
                                "{} совпадений в {} файлах · просмотрено {} (бинарных пропущено {}) · {:.2} с",
                                s.matches,
                                s.files_with_matches,
                                s.files_searched,
                                s.binary_skipped,
                                took.as_secs_f64()
                            )
                        }
                        Err(e) => self.error = Some(e),
                    }
                    done = true;
                }
            }
        }
        if dirty {
            self.groups.sort_by(|a, b| a.path.cmp(&b.path));
            self.flat.clear();
            for (gi, g) in self.groups.iter().enumerate() {
                self.flat.push((gi, None));
                for mi in 0..g.matches.len() {
                    self.flat.push((gi, Some(mi)));
                }
            }
        }
        if done {
            self.run = None;
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>, icons: &mut Icons) {
        let p = pal(ui);
        if self.start_now {
            self.start_now = false;
            self.start(shared, ui.ctx());
        }
        self.poll();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::heading(ui, "Поиск по содержимому файлов");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.pattern).hint_text("Что искать внутри файлов").desired_width(420.0).margin(vec2(10.0, 7.0)));
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.start(shared, ui.ctx());
                }
                ui.checkbox(&mut self.regex, "Регулярное выражение");
                ui.checkbox(&mut self.case_sensitive, "Учитывать регистр");
                if self.run.is_some() {
                    if ui.button("Остановить").clicked()
                        && let Some(r) = &self.run
                    {
                        r.progress.cancel();
                    }
                } else if theme::primary_button(ui, "Искать").clicked() {
                    self.start(shared, ui.ctx());
                }
            });
            ui.horizontal(|ui| {
                ui.label("Где:");
                ui.add(egui::TextEdit::singleline(&mut self.root).hint_text("папка (пусто — всё проиндексированное)").desired_width(300.0).margin(vec2(10.0, 6.0)));
                if ui.button("Обзор…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_folder()
                {
                    self.root = f.to_string_lossy().into_owned();
                }
                ui.label("Фильтр имён:");
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("ext:rs;toml  size:<1MB  modified:<30d").desired_width(300.0).margin(vec2(10.0, 6.0)));
            });
            if let Some(run) = &self.run {
                use std::sync::atomic::Ordering::Relaxed;
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().color(p.accent));
                    if run.candidates == 0 {
                        ui.label("Отбор файлов…");
                    } else {
                        ui.label(format!(
                            "Просмотрено {} из {} файлов · найдено {} · {:.1} с",
                            run.progress.done.load(Relaxed),
                            run.candidates,
                            run.progress.items.load(Relaxed),
                            run.started.elapsed().as_secs_f64()
                        ));
                    }
                });
                ui.ctx().request_repaint_after(Duration::from_millis(80));
            } else if let Some(e) = &self.error {
                ui.colored_label(p.err, e);
            } else if !self.status.is_empty() {
                ui.label(theme::dim(ui, &self.status));
            }
        });
        ui.add_space(8.0);
        self.results(ui, shared, icons);
    }

    fn open(&self, shared: &Shared, idx: usize) {
        let Some(&(gi, mi)) = self.flat.get(idx) else { return };
        let g = &self.groups[gi];
        let path = g.path.to_string_lossy().into_owned();
        match mi {
            Some(m) => actions::open_at_line(&shared.settings().editor, &path, g.matches[m].line),
            None => actions::open_path(&path),
        }
    }

    fn results(&mut self, ui: &mut Ui, shared: &Arc<Shared>, icons: &mut Icons) {
        let p = pal(ui);
        let n = self.flat.len();
        if n == 0 {
            if self.run.is_none() && self.status.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(30.0);
                    ui.label(theme::dim(ui, "Введите текст и нажмите Enter. Бинарные файлы пропускаются, UTF-16 с BOM читается."));
                });
            }
            return;
        }
        let (up, down, enter) = ui.ctx().input_mut(|i| {
            (i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::Enter))
        });
        let mut scroll = None;
        if up && self.selected > 0 {
            self.selected -= 1;
            scroll = Some(self.selected);
        }
        if down && self.selected + 1 < n {
            self.selected += 1;
            scroll = Some(self.selected);
        }
        if enter && !ui.ctx().egui_wants_keyboard_input() {
            self.open(shared, self.selected);
        }
        let row_h = 26.0;
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(s) = scroll {
            area = area.vertical_scroll_offset((s as f32 * (row_h + ui.spacing().item_spacing.y) - 120.0).max(0.0));
        }
        let mut clicked = None;
        let mut dbl = None;
        let selected = self.selected;
        area.show_rows(ui, row_h, n, |ui, range| {
            for i in range {
                let (gi, mi) = self.flat[i];
                let g = &self.groups[gi];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                if i == selected {
                    ui.painter().rect_filled(rect, 6.0, p.accent.gamma_multiply(0.2));
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 6.0, p.card_hover);
                }
                let painter = ui.painter_at(rect);
                match mi {
                    None => {
                        let name = g.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if let Some(t) = icons.get(ui.ctx(), &name, false) {
                            painter.image(t.id(), egui::Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - 9.0), vec2(18.0, 18.0)), egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        }
                        let galley = painter.layout_no_wrap(name, FontId::proportional(14.0), p.text);
                        let w = galley.size().x;
                        painter.galley(pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
                        let dir = g.path.parent().map(winapi::display_path).unwrap_or_default();
                        painter.text(pos2(rect.left() + 44.0 + w, rect.center().y), egui::Align2::LEFT_CENTER, format!("{dir}   · {}", g.matches.len()), FontId::proportional(12.0), p.dim);
                    }
                    Some(m) => {
                        let mt = &g.matches[m];
                        painter.text(pos2(rect.left() + 64.0, rect.center().y), egui::Align2::RIGHT_CENTER, mt.line.to_string(), FontId::monospace(12.5), p.dim);
                        let mut job = LayoutJob::default();
                        let normal = egui::TextFormat { font_id: FontId::monospace(13.0), color: p.text, ..Default::default() };
                        let hit = egui::TextFormat { font_id: FontId::monospace(13.0), color: Color32::WHITE, background: p.accent.gamma_multiply(0.85), ..Default::default() };
                        let t = mt.text.trim_start_matches([' ', '\t']);
                        let trimmed = mt.text.len() - t.len();
                        let mut pos = 0usize;
                        for &(a, b) in &mt.ranges {
                            let (a, b) = (a.saturating_sub(trimmed), b.saturating_sub(trimmed));
                            if a < pos || b > t.len() || !t.is_char_boundary(a) || !t.is_char_boundary(b) {
                                continue;
                            }
                            job.append(&t[pos..a], 0.0, normal.clone());
                            job.append(&t[a..b], 0.0, hit.clone());
                            pos = b;
                        }
                        job.append(&t[pos..], 0.0, normal);
                        job.wrap.max_width = f32::INFINITY;
                        job.wrap.max_rows = 1;
                        let galley = painter.layout_job(job);
                        painter.galley(pos2(rect.left() + 76.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
                    }
                }
                if resp.clicked() || resp.secondary_clicked() {
                    clicked = Some(i);
                }
                if resp.double_clicked() {
                    dbl = Some(i);
                }
                let sh = shared.clone();
                let path = g.path.clone();
                resp.context_menu(|ui| {
                    if actions::path_menu(ui, &sh, std::slice::from_ref(&path)) {
                        ui.close();
                    }
                });
            }
        });
        if let Some(i) = clicked {
            self.selected = i;
        }
        if let Some(i) = dbl {
            self.open(shared, i);
        }
        let _ = (RichText::new(""), util::bytes(0), PathBuf::new());
    }
}

fn candidates(sh: &Arc<Shared>, root: &str, filter: &str, progress: &Progress) -> Result<Vec<PathBuf>, String> {
    let st = sh.backend.status();
    let covered = |r: &str| {
        let n = strela_core::guard::normalize(std::path::Path::new(r));
        st.roots.iter().any(|x| {
            let xr = strela_core::guard::normalize(std::path::Path::new(&x.root));
            n == xr || (n.starts_with(&xr) && (xr.ends_with('\\') || n.as_bytes().get(xr.len()) == Some(&b'\\')))
        })
    };
    if root.is_empty() {
        if filter.trim().is_empty() {
            return Err("Укажите папку или фильтр имён (например ext:rs) — иначе придётся читать все файлы всех дисков".into());
        }
        let (files, truncated) = sh.backend.files(filter, 1_000_000)?;
        if truncated {
            return Err("Слишком много файлов подходит под фильтр: сузьте его или укажите папку".into());
        }
        return Ok(files);
    }
    if covered(root) {
        let f = format!("path:\"{root}\" {filter}");
        if let Ok((files, false)) = sh.backend.files(&f, 2_000_000) {
            return Ok(files);
        }
    }
    grep::candidates_from_walk(std::path::Path::new(root), filter, progress).map_err(|e| e.to_string())
}
