//! Name search: a background worker that always answers the latest query, and the result list used by both the
//! "Поиск" tab and the Spotlight popup.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, FontId, Key, Modifiers, Rect, Sense, Ui, pos2, vec2};
use strela_core::ipc::RowDto;
use strela_core::util;

use super::icons::Icons;
use super::theme::{self, pal};
use super::actions;
use crate::backend::SearchOut;
use crate::state::Shared;

struct Req {
    seq: u64,
    query: String,
    regex: bool,
    fuzzy: bool,
    limit: usize,
}

struct Done {
    seq: u64,
    out: Result<SearchOut, String>,
}

pub struct SearchPane {
    shared: Arc<Shared>,
    pub query: String,
    sent: Option<(String, bool, bool, u64)>,
    seq: u64,
    tx: Sender<Req>,
    inbox: Arc<Mutex<Option<Done>>>,
    pub rows: Vec<RowDto>,
    pub total: usize,
    pub elapsed_us: u64,
    pub error: Option<String>,
    pub selected: usize,
    pub busy: bool,
    pub focus: bool,
    visible: std::ops::Range<usize>,
    scroll_to: Option<f32>,
    limit: usize,
    /// Bumped by the owner when the index changed under the current query.
    pub index_stamp: u64,
}

impl SearchPane {
    pub fn new(shared: Arc<Shared>, limit: usize) -> SearchPane {
        let (tx, rx): (Sender<Req>, Receiver<Req>) = channel();
        let inbox: Arc<Mutex<Option<Done>>> = Arc::new(Mutex::new(None));
        {
            let (inbox, sh) = (inbox.clone(), shared.clone());
            std::thread::Builder::new()
                .name("strela-search".into())
                .spawn(move || {
                    while let Ok(mut req) = rx.recv() {
                        // only the newest request matters
                        while let Ok(newer) = rx.try_recv() {
                            req = newer;
                        }
                        let out = sh.backend.search(&req.query, req.regex, req.fuzzy, req.limit);
                        *inbox.lock().unwrap() = Some(Done { seq: req.seq, out });
                        sh.repaint();
                    }
                })
                .expect("spawn search worker");
        }
        SearchPane {
            shared,
            query: String::new(),
            sent: None,
            seq: 0,
            tx,
            inbox,
            rows: Vec::new(),
            total: 0,
            elapsed_us: 0,
            error: None,
            selected: 0,
            busy: false,
            focus: false,
            visible: 0..0,
            scroll_to: None,
            limit,
            index_stamp: 0,
        }
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
        self.selected = 0;
    }

    /// Sends a request when the query or flags changed and picks up the newest answer.
    pub fn poll(&mut self) {
        let s = self.shared.settings();
        let key = (self.query.clone(), s.regex, s.fuzzy, self.index_stamp);
        if self.sent.as_ref() != Some(&key) {
            self.sent = Some(key);
            if self.query.trim().is_empty() {
                self.rows.clear();
                self.total = 0;
                self.error = None;
                self.busy = false;
                self.seq += 1; // invalidates answers still on the way
            } else {
                self.seq += 1;
                self.busy = true;
                let _ = self.tx.send(Req { seq: self.seq, query: self.query.clone(), regex: s.regex, fuzzy: s.fuzzy, limit: self.limit });
            }
        }
        if let Some(d) = self.inbox.lock().unwrap().take()
            && d.seq == self.seq
        {
            self.busy = false;
            match d.out {
                Ok(o) => {
                    let keep = self.selected.min(o.rows.len().saturating_sub(1));
                    self.rows = o.rows;
                    self.total = o.total;
                    self.elapsed_us = o.elapsed_us;
                    self.error = None;
                    self.selected = keep;
                }
                Err(e) => {
                    self.rows.clear();
                    self.total = 0;
                    self.error = Some(e);
                }
            }
        }
    }

    /// Up/Down/PageUp/PageDown/Home/End; Enter opens, Ctrl+Enter shows the folder, Del / Shift+Del delete.
    /// Returns true when something was opened.
    pub fn keys(&mut self, ctx: &egui::Context) -> bool {
        let n = self.rows.len();
        let mut opened = false;
        let (up, down, pgup, pgdn, home, end, enter, ctrl_enter, del, shift_del) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
                i.consume_key(Modifiers::CTRL, Key::Home),
                i.consume_key(Modifiers::CTRL, Key::End),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::CTRL, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Delete),
                i.consume_key(Modifiers::SHIFT, Key::Delete),
            )
        });
        if n > 0 {
            let old = self.selected;
            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            if down {
                self.selected = (self.selected + 1).min(n - 1);
            }
            if pgup {
                self.selected = self.selected.saturating_sub(10);
            }
            if pgdn {
                self.selected = (self.selected + 10).min(n - 1);
            }
            if home {
                self.selected = 0;
            }
            if end {
                self.selected = n - 1;
            }
            if old != self.selected {
                self.scroll_to = Some(self.selected as f32);
            }
            if let Some(r) = self.rows.get(self.selected).cloned() {
                if ctrl_enter {
                    actions::reveal(&r.path);
                    opened = true;
                } else if enter {
                    actions::open_path(&r.path);
                    opened = true;
                }
                let paths = vec![PathBuf::from(&r.path)];
                if shift_del {
                    crate::commands::execute(&self.shared, crate::commands::Command { verb: "fast-delete".into(), paths: vec![r.path.clone()], dest: None });
                } else if del && !ctx.egui_wants_keyboard_input() {
                    let _ = paths;
                    crate::commands::execute(&self.shared, crate::commands::Command { verb: "recycle".into(), paths: vec![r.path.clone()], dest: None });
                }
            }
        }
        opened
    }

    /// The result list. Returns true if a result was opened (double click / Enter handled elsewhere).
    pub fn list_ui(&mut self, ui: &mut Ui, icons: &mut Icons, row_h: f32, max_rows: Option<usize>) -> bool {
        let p = pal(ui);
        let mut opened = false;
        if self.rows.is_empty() {
            let msg = if let Some(e) = &self.error {
                format!("Ошибка запроса: {e}")
            } else if self.query.trim().is_empty() {
                "Начните вводить имя файла. Фильтры: ext:mp4  size:>1GB  modified:<7d  path:D:\\Games".to_string()
            } else if self.busy {
                "Поиск…".to_string()
            } else {
                "Ничего не найдено".to_string()
            };
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.label(theme::dim(ui, msg)));
            return false;
        }
        let n = self.rows.len().min(max_rows.unwrap_or(usize::MAX));
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, max_rows.is_some()]);
        if let Some(target) = self.scroll_to.take() {
            let (lo, hi) = (self.visible.start as f32, self.visible.end as f32);
            if target < lo + 1.0 || target > hi - 2.0 {
                area = area.vertical_scroll_offset((target - 3.0).max(0.0) * (row_h + ui.spacing().item_spacing.y));
            }
        }
        let shared = self.shared.clone();
        let mut new_selected = None;
        let selected = self.selected;
        let out = area.show_rows(ui, row_h, n, |ui, range| {
            for i in range {
                let row = &self.rows[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                if i == selected {
                    ui.painter().rect_filled(rect, 8.0, p.accent.gamma_multiply(0.22));
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 8.0, p.card_hover);
                }
                if resp.clicked() || resp.secondary_clicked() {
                    new_selected = Some(i);
                }
                if resp.double_clicked() {
                    actions::open_path(&row.path);
                    opened = true;
                }
                draw_row(ui, rect, row, icons, &p, row_h);
                resp.context_menu(|ui| {
                    if actions::path_menu(ui, &shared, &[PathBuf::from(&row.path)]) {
                        ui.close();
                    }
                });
            }
        });
        let per = row_h + ui.spacing().item_spacing.y;
        let first = (out.state.offset.y / per).floor().max(0.0) as usize;
        let count = (out.inner_rect.height() / per).ceil() as usize + 1;
        self.visible = first..(first + count).min(n);
        if let Some(i) = new_selected {
            self.selected = i;
        }
        opened
    }

    pub fn summary(&self) -> String {
        if self.query.trim().is_empty() {
            return String::new();
        }
        let shown = self.rows.len();
        let ms = self.elapsed_us as f64 / 1000.0;
        if self.total > shown {
            format!("{} совпадений (показано {shown}) · {ms:.1} мс", self.total)
        } else {
            format!("{} совпадений · {ms:.1} мс", self.total)
        }
    }
}

fn draw_row(ui: &Ui, rect: Rect, row: &RowDto, icons: &mut Icons, p: &theme::Palette, row_h: f32) {
    let painter = ui.painter_at(rect);
    let icon_size = (row_h - 12.0).clamp(16.0, 28.0);
    let icon_rect = Rect::from_min_size(pos2(rect.left() + 10.0, rect.center().y - icon_size / 2.0), vec2(icon_size, icon_size));
    if let Some(tex) = icons.get(ui.ctx(), &row.name, row.is_dir) {
        painter.image(tex.id(), icon_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    }
    let x0 = icon_rect.right() + 10.0;
    let right = rect.right() - 12.0;
    let date_w = 118.0;
    let size_w = 78.0;
    let date_x = right - date_w;
    let size_x = date_x - size_w;
    let text_w = (size_x - x0 - 10.0).max(60.0);
    let name_w = (text_w * 0.42).max(120.0).min(text_w);
    let name_galley = painter.layout(row.name.clone(), FontId::proportional(14.5), p.text, name_w);
    painter.galley(pos2(x0, rect.center().y - name_galley.size().y.min(row_h) / 2.0), name_galley, p.text);
    let parent = row.path.rsplit_once('\\').map_or("", |(a, _)| a).to_string();
    let path_x = x0 + name_w + 10.0;
    let path_w = (size_x - path_x - 10.0).max(0.0);
    if path_w > 20.0 {
        let clipped = painter.with_clip_rect(Rect::from_min_max(pos2(path_x, rect.top()), pos2(path_x + path_w, rect.bottom())));
        // keep the tail of long paths: it is the informative part
        let g = clipped.layout_no_wrap(parent, FontId::proportional(12.5), p.dim);
        let x = if g.size().x > path_w { path_x + path_w - g.size().x } else { path_x };
        clipped.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, p.dim);
    }
    let size = if row.is_dir { "папка".to_string() } else { util::bytes(row.size) };
    painter.text(pos2(date_x - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, size, FontId::proportional(12.5), p.dim);
    painter.text(pos2(right, rect.center().y), egui::Align2::RIGHT_CENTER, util::date(row.mtime), FontId::proportional(12.5), p.dim);
}

