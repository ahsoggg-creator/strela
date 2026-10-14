//! A small table toolkit for the system tabs: column layout, a clickable sortable header and clipped cell text.

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Ui, pos2, vec2};

use super::theme::pal;

#[derive(Clone, Copy)]
pub struct Col {
    pub title: &'static str,
    /// Fixed width; `0.0` shares what is left with the other flexible columns.
    pub width: f32,
    pub right: bool,
}

pub const fn col(title: &'static str, width: f32) -> Col {
    Col { title, width, right: false }
}

pub const fn rcol(title: &'static str, width: f32) -> Col {
    Col { title, width, right: true }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub col: usize,
    pub desc: bool,
}

/// Left edge and width of each column inside a row of `total` width (with 8 px padding on both sides).
pub fn layout(total: f32, cols: &[Col]) -> Vec<(f32, f32)> {
    let fixed: f32 = cols.iter().map(|c| c.width).sum();
    let flex = cols.iter().filter(|c| c.width <= 0.0).count().max(1) as f32;
    let each = ((total - 16.0 - fixed) / flex).max(60.0);
    let mut x = 8.0;
    cols.iter()
        .map(|c| {
            let w = if c.width > 0.0 { c.width } else { each };
            let r = (x, w);
            x += w;
            r
        })
        .collect()
}

/// Header row; clicking a title sorts by it (twice: reverses). Returns true when the sort changed.
pub fn header(ui: &mut Ui, cols: &[Col], sort: &mut Sort) -> bool {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    ui.painter().rect_filled(rect, 6.0, p.panel);
    let xs = layout(rect.width(), cols);
    let mut changed = false;
    for (i, (c, (x, w))) in cols.iter().zip(&xs).enumerate() {
        let cell = Rect::from_min_size(pos2(rect.left() + x, rect.top()), vec2(*w, rect.height()));
        let resp = ui.interact(cell, ui.id().with(("hdr", i)), Sense::click());
        if resp.clicked() {
            if sort.col == i {
                sort.desc = !sort.desc;
            } else {
                *sort = Sort { col: i, desc: false };
            }
            changed = true;
        }
        let arrow = if sort.col == i { if sort.desc { " ▼" } else { " ▲" } } else { "" };
        let color = if sort.col == i || resp.hovered() { p.text } else { p.dim };
        let (anchor, px) = if c.right { (Align2::RIGHT_CENTER, cell.right() - 6.0) } else { (Align2::LEFT_CENTER, cell.left() + 2.0) };
        ui.painter_at(cell).text(pos2(px, cell.center().y), anchor, format!("{}{arrow}", c.title), FontId::proportional(12.0), color);
    }
    changed
}

/// Text clipped to its column.
pub fn cell(ui: &Ui, row: Rect, col: &Col, pos: (f32, f32), text: &str, color: Color32, size: f32) {
    let cell = Rect::from_min_size(pos2(row.left() + pos.0, row.top()), vec2(pos.1, row.height()));
    let (anchor, px) = if col.right { (Align2::RIGHT_CENTER, cell.right() - 6.0) } else { (Align2::LEFT_CENTER, cell.left() + 2.0) };
    ui.painter_at(cell).text(pos2(px, row.center().y), anchor, text, FontId::proportional(size), color);
}

/// A flat checkbox-like switch painted in a row; returns true when clicked.
pub fn switch(ui: &mut Ui, at: Rect, on: bool, enabled: bool) -> bool {
    let p = pal(ui);
    let resp = ui.interact(at, ui.id().with(("sw", at.min.x as i32, at.min.y as i32)), if enabled { Sense::click() } else { Sense::hover() });
    let t = ui.ctx().animate_bool_with_time(resp.id, on, 0.12);
    let track = Rect::from_center_size(at.center(), vec2(30.0, 16.0));
    let fill = super::theme::lerp(p.border, p.accent, t);
    ui.painter().rect_filled(track, 99.0, if enabled { fill } else { fill.gamma_multiply(0.5) });
    let knob_x = track.left() + 8.0 + 14.0 * t;
    ui.painter().circle_filled(pos2(knob_x, track.center().y), 6.0, Color32::WHITE);
    resp.clicked()
}

pub fn filter_box(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> egui::Response {
    ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(width).margin(vec2(10.0, 6.0)))
}

/// Row background: hover and selection.
pub fn row_bg(ui: &Ui, rect: Rect, hovered: bool, selected: bool) {
    let p = pal(ui);
    if selected {
        ui.painter().rect_filled(rect, 6.0, p.accent.gamma_multiply(0.18));
    } else if hovered {
        ui.painter().rect_filled(rect, 6.0, p.card_hover.gamma_multiply(0.6));
    }
}

/// Case-insensitive "all words appear" test.
pub fn matches(filter: &str, hay: &[&str]) -> bool {
    let f = filter.trim().to_lowercase();
    if f.is_empty() {
        return true;
    }
    let joined = hay.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>().join("\u{1}");
    f.split_whitespace().all(|w| joined.contains(w))
}
