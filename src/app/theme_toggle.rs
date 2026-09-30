//! The button in the top-right corner that turns dark mode on and off: a moon
//! in light mode, a sun in dark mode.

use std::f32::consts::{FRAC_PI_4, TAU};

use eframe::egui::{self, epaint::PathShape, vec2, Color32, Mesh, Painter, Pos2, Sense, Shape, Stroke, Vec2};

const SUN: Color32 = Color32::from_rgb(246, 208, 112);

/// Shows what a click switches to. egui saves the theme with the rest of its
/// state, so the choice sticks between launches.
pub fn button(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, dark, "Dark mode"));
    let resp = resp.on_hover_text(if dark { "Switch to light mode" } else { "Switch to dark mode" });
    if resp.clicked() {
        ui.ctx().set_theme(if dark { egui::Theme::Light } else { egui::Theme::Dark });
    }
    if !ui.is_rect_visible(rect) {
        return;
    }

    // 0 = moon, 1 = sun; the two cross-fade and trade places in size.
    let t = ui.ctx().animate_bool_responsive(resp.id, dark);
    let v = ui.visuals();
    let painter = ui.painter();
    let c = rect.center();
    if resp.hovered() {
        painter.circle_filled(c, 13.0, v.widgets.hovered.weak_bg_fill);
    }
    let ink = if resp.hovered() { v.strong_text_color() } else { v.weak_text_color() };
    if t < 1.0 {
        let color = ink.gamma_multiply(1.0 - t);
        let (outer, inner) = crescent(c, 7.0 * (1.0 - 0.4 * t), -FRAC_PI_4 - 0.6 * t);
        fill_crescent(painter, outer, inner, color);
    }
    if t > 0.0 {
        let color = if resp.hovered() { ink.lerp_to_gamma(SUN, 0.7) } else { SUN }.gamma_multiply(t);
        sun(painter, c, 0.6 + 0.4 * t, (1.0 - t) * FRAC_PI_4, color);
    }
}

fn sun(painter: &Painter, c: Pos2, scale: f32, turn: f32, color: Color32) {
    painter.circle_filled(c, 3.4 * scale, color);
    for k in 0..8 {
        let d = Vec2::angled(turn + k as f32 * TAU / 8.0);
        painter.line_segment([c + d * 5.6 * scale, c + d * 7.6 * scale], Stroke::new(1.6 * scale, color));
    }
}

fn fill_crescent(painter: &Painter, outer: Vec<Pos2>, inner: Vec<Pos2>, color: Color32) {
    let mut mesh = Mesh::default();
    for (o, i) in outer.iter().zip(&inner) {
        mesh.colored_vertex(*o, color);
        mesh.colored_vertex(*i, color);
    }
    for k in (0..outer.len() as u32 - 1).map(|k| 2 * k) {
        mesh.add_triangle(k, k + 1, k + 2);
        mesh.add_triangle(k + 1, k + 3, k + 2);
    }
    painter.add(Shape::mesh(mesh));
    // The mesh has hard edges; tracing the outline smooths them.
    let mut edge = outer;
    edge.extend(inner[1..inner.len() - 1].iter().rev());
    painter.add(PathShape::closed_line(edge, Stroke::new(1.0, color)));
}

/// A circle of radius `r` with a bite taken out in direction `dir`, as its two
/// edges: the outer arc and the inner arc, both running tip to tip.
fn crescent(center: Pos2, r: f32, dir: f32) -> (Vec<Pos2>, Vec<Pos2>) {
    const STEPS: usize = 28;
    let (r2, d) = (r * 0.82, r * 0.62);
    let u = Vec2::angled(dir);
    // Shift away from the bite so the lit part sits in the middle of the button.
    let c = center - u * (r * 0.18);
    // Where the two circles cross, as angles around each center.
    let a = (r * r - r2 * r2 + d * d) / (2.0 * d);
    let h = (r * r - a * a).sqrt();
    let alpha = h.atan2(a);
    let beta = h.atan2(a - d);
    let arc = |center: Pos2, radius: f32, from: f32, to: f32| -> Vec<Pos2> {
        (0..=STEPS)
            .map(|i| center + Vec2::angled(from + (to - from) * i as f32 / STEPS as f32) * radius)
            .collect()
    };
    let outer = arc(c, r, dir + alpha, dir + TAU - alpha);
    let inner = arc(c + u * d, r2, dir + beta, dir + TAU - beta);
    (outer, inner)
}
