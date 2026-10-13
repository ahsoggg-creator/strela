//! Look & feel: palettes for both themes, accent color, rounded cards, pills, animated progress and the speed chart.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Pos2, Rect, Response, RichText, Sense,
    Shape, Stroke, StrokeKind, Theme, Ui, pos2, vec2,
};

use crate::settings::{Settings, ThemeChoice};

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
}

pub fn palette(dark: bool, accent: [u8; 3]) -> Palette {
    let accent = Color32::from_rgb(accent[0], accent[1], accent[2]);
    if dark {
        Palette {
            bg: Color32::from_rgb(20, 22, 27),
            panel: Color32::from_rgb(26, 29, 35),
            card: Color32::from_rgb(33, 37, 45),
            card_hover: Color32::from_rgb(41, 46, 56),
            border: Color32::from_rgb(52, 58, 70),
            text: Color32::from_rgb(232, 235, 240),
            dim: Color32::from_rgb(145, 153, 168),
            accent,
            ok: Color32::from_rgb(74, 200, 120),
            warn: Color32::from_rgb(235, 175, 60),
            err: Color32::from_rgb(235, 90, 90),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(243, 245, 249),
            panel: Color32::from_rgb(250, 251, 253),
            card: Color32::WHITE,
            card_hover: Color32::from_rgb(238, 242, 248),
            border: Color32::from_rgb(218, 223, 232),
            text: Color32::from_rgb(28, 32, 40),
            dim: Color32::from_rgb(107, 116, 132),
            accent,
            ok: Color32::from_rgb(30, 150, 80),
            warn: Color32::from_rgb(200, 130, 10),
            err: Color32::from_rgb(210, 55, 55),
        }
    }
}

pub fn pal(ui: &Ui) -> Palette {
    // the accent is stored in the selection stroke color so widgets need no access to the settings
    let dark = ui.visuals().dark_mode;
    palette(dark, {
        let c = ui.visuals().selection.stroke.color;
        [c.r(), c.g(), c.b()]
    })
}

/// Windows' system theme is followed by egui itself (`ThemePreference::System`).
pub fn install(ctx: &egui::Context, s: &Settings) {
    ctx.options_mut(|o| {
        o.theme_preference = match s.theme {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
        }
    });
    for theme in [Theme::Dark, Theme::Light] {
        let p = palette(theme == Theme::Dark, s.accent);
        ctx.style_mut_of(theme, |st| {
            st.spacing.item_spacing = vec2(8.0, 7.0);
            st.spacing.button_padding = vec2(12.0, 6.0);
            st.spacing.menu_margin = Margin::same(8);
            st.spacing.interact_size.y = 28.0;
            st.spacing.scroll.bar_width = 8.0;
            let v = &mut st.visuals;
            v.panel_fill = p.bg;
            v.window_fill = p.card;
            v.extreme_bg_color = p.panel;
            v.faint_bg_color = p.panel;
            v.window_corner_radius = CornerRadius::same(14);
            v.menu_corner_radius = CornerRadius::same(10);
            v.window_stroke = Stroke::new(1.0, p.border);
            v.override_text_color = Some(p.text);
            v.selection.bg_fill = p.accent.gamma_multiply(0.30);
            v.selection.stroke = Stroke::new(1.0, p.accent);
            v.hyperlink_color = p.accent;
            let r = CornerRadius::same(6);
            for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
                w.corner_radius = r;
            }
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
            v.widgets.inactive.bg_fill = p.card;
            v.widgets.inactive.weak_bg_fill = p.card;
            v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
            v.widgets.hovered.bg_fill = p.card_hover;
            v.widgets.hovered.weak_bg_fill = p.card_hover;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.7));
            v.widgets.active.bg_fill = p.accent.gamma_multiply(0.35);
            v.widgets.active.weak_bg_fill = p.accent.gamma_multiply(0.35);
        });
    }
}

/// Segoe UI (the Windows UI font) in front of egui's built-in fonts, Segoe UI Symbol as a glyph fallback.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    for (name, file, front) in [("segoe", "segoeui.ttf", true), ("segoe-symbols", "seguisym.ttf", false)] {
        if let Ok(bytes) = std::fs::read(format!(r"{windir}\Fonts\{file}")) {
            fonts.font_data.insert(name.into(), std::sync::Arc::new(FontData::from_owned(bytes)));
            for fam in [FontFamily::Proportional, FontFamily::Monospace] {
                let list = fonts.families.entry(fam.clone()).or_default();
                if front && fam == FontFamily::Proportional {
                    list.insert(0, name.into());
                } else {
                    list.push(name.into());
                }
            }
        }
    }
    ctx.set_fonts(fonts);
}

pub fn card(ui: &Ui) -> egui::Frame {
    let p = pal(ui);
    egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.border)).corner_radius(14).inner_margin(Margin::same(16))
}

pub fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(17.0).strong());
}

pub fn dim(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(pal(ui).dim).size(12.5)
}

pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(p.accent).corner_radius(10).min_size(vec2(0.0, 32.0)))
}

pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(p.err).corner_radius(10).min_size(vec2(0.0, 32.0)))
}

/// Rounded chip with a colored background.
pub fn chip(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(11.5), Color32::WHITE);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(14.0, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 99.0, color.gamma_multiply(0.9));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    resp
}

/// Pill-shaped tab; the highlight animates between tabs.
pub fn tab_pill(ui: &mut Ui, label: &str, selected: bool) -> Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(14.0), p.text);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(28.0, 14.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, selected, 0.18);
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
    let bg = if selected || t > 0.0 {
        p.accent.gamma_multiply(0.18 + 0.12 * t)
    } else {
        p.card_hover.gamma_multiply(hover)
    };
    ui.painter().rect_filled(rect, 99.0, bg);
    let color = lerp(p.dim, p.accent, t);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(14.0), color);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()), m(a.a(), b.a()))
}

/// Progress bar with smooth value changes and a moving highlight while `active`.
pub fn progress_bar(ui: &mut Ui, id: egui::Id, frac: f32, active: bool, color: Color32, height: f32) {
    let p = pal(ui);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let shown = ui.ctx().animate_value_with_time(id, frac.clamp(0.0, 1.0), 0.25);
    ui.painter().rect_filled(rect, height / 2.0, p.panel);
    ui.painter().rect_stroke(rect, height / 2.0, Stroke::new(1.0, p.border), StrokeKind::Inside);
    if shown > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * shown).max(height));
        ui.painter().rect_filled(fill, height / 2.0, color);
        if active {
            let t = (ui.input(|i| i.time) * 0.9).fract() as f32;
            let w = (fill.width() * 0.35).max(24.0);
            let x = fill.left() - w + (fill.width() + w) * t;
            let glow = Rect::from_min_max(pos2(x.max(fill.left()), fill.top()), pos2((x + w).min(fill.right()), fill.bottom()));
            if glow.width() > 0.0 {
                ui.painter().rect_filled(glow, height / 2.0, Color32::WHITE.gamma_multiply(0.22));
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        }
    }
}

/// Live speed chart: filled area + line, scaled to the recent maximum.
pub fn speed_chart(ui: &mut Ui, history: &[f32], height: f32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter().rect_filled(rect, 10.0, p.panel);
    ui.painter().rect_stroke(rect, 10.0, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let max = history.iter().cloned().fold(1.0f32, f32::max) * 1.15;
    let inner = rect.shrink2(vec2(10.0, 8.0));
    for i in 1..4 {
        let y = inner.bottom() - inner.height() * i as f32 / 4.0;
        ui.painter().line_segment([pos2(inner.left(), y), pos2(inner.right(), y)], Stroke::new(1.0, p.border.gamma_multiply(0.6)));
    }
    if history.len() >= 2 {
        let n = history.len();
        let pts: Vec<Pos2> = history
            .iter()
            .enumerate()
            .map(|(i, v)| pos2(inner.left() + inner.width() * i as f32 / (n - 1) as f32, inner.bottom() - inner.height() * (v / max).clamp(0.0, 1.0)))
            .collect();
        let mut area = pts.clone();
        area.push(pos2(inner.right(), inner.bottom()));
        area.push(pos2(inner.left(), inner.bottom()));
        ui.painter().add(Shape::convex_polygon(area, p.accent.gamma_multiply(0.18), Stroke::NONE));
        ui.painter().add(Shape::line(pts, Stroke::new(2.0, p.accent)));
    }
    ui.painter().text(
        pos2(inner.left() + 2.0, inner.top()),
        Align2::LEFT_TOP,
        strela_core::util::speed(max as f64 / 1.15),
        FontId::proportional(11.0),
        p.dim,
    );
}

/// Horizontal share bar behind a list row.
pub fn share_bar(ui: &Ui, rect: Rect, frac: f32, color: Color32) {
    let mut r = rect;
    r.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(2.0));
    ui.painter().rect_filled(r, 6.0, color.gamma_multiply(0.16));
}

/// Fade-in factor for freshly shown content.
pub fn appear(ui: &Ui, id: egui::Id) -> f32 {
    ui.ctx().animate_bool_with_time(id, true, 0.25)
}
