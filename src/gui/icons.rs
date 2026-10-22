//! Toolbar icons, drawn with simple shapes. They are our own drawings (no pictures
//! are loaded), so they follow the theme colours.

use eframe::egui::{self, Color32, Pos2, Rect, Response, Sense, Shape, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Play,
    Pause,
    Stop,
    ToTop,
    Up,
    Down,
    ToBottom,
    Clear,
}

const SIZE: Vec2 = Vec2::new(30.0, 28.0);

/// A square icon button with a tooltip. Disabled buttons are dimmed and inert.
pub fn button(ui: &mut egui::Ui, icon: Icon, tooltip: &str, enabled: bool) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(SIZE, sense);
    if ui.is_rect_visible(rect) {
        if enabled && (response.hovered() || response.has_focus()) {
            let visuals = ui.style().interact(&response);
            ui.painter().rect_filled(rect, 4.0, visuals.bg_fill);
            ui.painter()
                .rect_stroke(rect, 4.0, visuals.bg_stroke, egui::StrokeKind::Inside);
        }
        let color = if enabled {
            accent(ui.visuals().dark_mode, icon).unwrap_or_else(|| ui.visuals().text_color())
        } else {
            ui.visuals().weak_text_color()
        };
        paint(ui.painter(), rect.shrink2(Vec2::new(7.0, 6.0)), icon, color);
    }
    response.on_hover_text(tooltip)
}

fn pt(rect: Rect, x: f32, y: f32) -> Pos2 {
    Pos2::new(
        rect.left() + rect.width() * x,
        rect.top() + rect.height() * y,
    )
}

/// Play is green and stop is red; everything else uses the text colour.
fn accent(dark: bool, kind: Icon) -> Option<Color32> {
    let (green, red) = if dark {
        (
            Color32::from_rgb(0x4a, 0xde, 0x80),
            Color32::from_rgb(0xf8, 0x71, 0x71),
        )
    } else {
        (
            Color32::from_rgb(0x15, 0x80, 0x3d),
            Color32::from_rgb(0xb9, 0x1c, 0x1c),
        )
    };
    match kind {
        Icon::Play => Some(green),
        Icon::Stop => Some(red),
        _ => None,
    }
}

/// An arrow head at `tip` whose wings reach towards `towards` (y of the wing tips).
fn arrow_head(p: &egui::Painter, r: Rect, tip_y: f32, wing_y: f32, stroke: Stroke) {
    p.line_segment([pt(r, 0.5, tip_y), pt(r, 0.15, wing_y)], stroke);
    p.line_segment([pt(r, 0.5, tip_y), pt(r, 0.85, wing_y)], stroke);
}

fn paint(p: &egui::Painter, r: Rect, icon: Icon, color: Color32) {
    let stroke = Stroke::new(2.0, color);
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(
                vec![pt(r, 0.15, 0.0), pt(r, 1.0, 0.5), pt(r, 0.15, 1.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            p.rect_filled(
                Rect::from_min_max(pt(r, 0.1, 0.0), pt(r, 0.4, 1.0)),
                1.0,
                color,
            );
            p.rect_filled(
                Rect::from_min_max(pt(r, 0.6, 0.0), pt(r, 0.9, 1.0)),
                1.0,
                color,
            );
        }
        Icon::Stop => {
            p.rect_filled(
                Rect::from_min_max(pt(r, 0.05, 0.05), pt(r, 0.95, 0.95)),
                2.0,
                color,
            );
        }
        Icon::Up => {
            p.line_segment([pt(r, 0.5, 0.05), pt(r, 0.5, 0.95)], stroke);
            arrow_head(p, r, 0.05, 0.45, stroke);
        }
        Icon::Down => {
            p.line_segment([pt(r, 0.5, 0.05), pt(r, 0.5, 0.95)], stroke);
            arrow_head(p, r, 0.95, 0.55, stroke);
        }
        Icon::ToTop => {
            p.line_segment([pt(r, 0.1, 0.05), pt(r, 0.9, 0.05)], stroke);
            p.line_segment([pt(r, 0.5, 0.25), pt(r, 0.5, 0.95)], stroke);
            arrow_head(p, r, 0.25, 0.6, stroke);
        }
        Icon::ToBottom => {
            p.line_segment([pt(r, 0.1, 0.95), pt(r, 0.9, 0.95)], stroke);
            p.line_segment([pt(r, 0.5, 0.05), pt(r, 0.5, 0.75)], stroke);
            arrow_head(p, r, 0.75, 0.4, stroke);
        }
        Icon::Clear => {
            p.line_segment([pt(r, 0.1, 0.1), pt(r, 0.9, 0.9)], stroke);
            p.line_segment([pt(r, 0.9, 0.1), pt(r, 0.1, 0.9)], stroke);
        }
    }
}
