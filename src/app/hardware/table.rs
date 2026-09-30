//! Every sensor in one table, like HWMonitor: device, then each kind of
//! reading, with current value, minimum, maximum, average and a trend line.

use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, RichText, Sense};

use super::draw;
use super::{short_name, ViewState};
use crate::sensors::{Kind, Snapshot};

const ROW: f32 = 22.0;
const NUM_W: f32 = 104.0;
const TREND_W: f32 = 110.0;

pub fn show(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState) {
    let p = draw::palette(ui);
    let width = ui.available_width();
    // Narrow windows drop the average, then the trend.
    let show_avg = width > 640.0;
    let show_trend = width > 520.0;
    let cols = [("Value", true), ("Min", true), ("Max", true), ("Average", show_avg)];
    let numbers_w = cols.iter().filter(|c| c.1).count() as f32 * NUM_W + if show_trend { TREND_W + 12.0 } else { 0.0 };

    // Column headings.
    let (rect, _) = ui.allocate_exact_size(vec2(width, ROW), Sense::hover());
    let font = FontId::proportional(12.0);
    let painter = ui.painter();
    painter.text(pos2(rect.left() + 4.0, rect.center().y), Align2::LEFT_CENTER, "Sensor", font.clone(), p.muted);
    let mut x = rect.right() - if show_trend { TREND_W + 12.0 } else { 0.0 };
    for (name, on) in cols.iter().rev() {
        if *on {
            painter.text(pos2(x - 8.0, rect.center().y), Align2::RIGHT_CENTER, *name, font.clone(), p.muted);
            x -= NUM_W;
        }
    }
    if show_trend {
        painter.text(pos2(rect.right() - TREND_W, rect.center().y), Align2::LEFT_CENTER, "Last 2 minutes", font.clone(), p.muted);
    }
    painter.line_segment([pos2(rect.left(), rect.bottom()), pos2(rect.right(), rect.bottom())], egui::Stroke::new(1.0, p.grid));

    for d in &snap.devices {
        let open = !vs.collapsed.contains(&d.key);
        let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW + 6.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let arrow = if open { "⏷" } else { "⏵" };
        ui.painter().text(pos2(rect.left() + 4.0, rect.center().y), Align2::LEFT_CENTER, arrow, font.clone(), p.muted);
        let title = ui.painter().text(
            pos2(rect.left() + 22.0, rect.center().y),
            Align2::LEFT_CENTER,
            short_name(&d.name),
            FontId::proportional(14.0),
            p.ink,
        );
        let detail = if d.detail.is_empty() { d.class.label().to_string() } else { format!("{} · {}", d.class.label(), d.detail) };
        ui.painter().text(pos2(title.right() + 10.0, rect.center().y), Align2::LEFT_CENTER, detail, font.clone(), p.muted);
        if resp.clicked() && !vs.collapsed.remove(&d.key) {
            vs.collapsed.insert(d.key.clone());
        }
        if !open {
            continue;
        }
        for kind in Kind::ALL {
            let sensors: Vec<_> = d.of_kind(kind).collect();
            if sensors.is_empty() {
                continue;
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(kind.group()).size(11.5).strong().color(p.muted));
            });
            for (n, s) in sensors.into_iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW), Sense::click());
                let focused = vs.focus.iter().any(|(dk, sk)| *dk == d.key && *sk == s.key);
                if focused {
                    ui.painter().rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.5));
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                } else if n % 2 == 1 {
                    // Striping keeps a long row readable from label to numbers.
                    ui.painter().rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
                }
                let painter = ui.painter();
                let ink = if s.value.is_some() { p.ink } else { p.muted };
                let label_rect = Rect::from_min_max(pos2(rect.left() + 34.0, rect.top()), pos2(rect.right() - numbers_w - 8.0, rect.bottom()));
                let clipped = painter.with_clip_rect(label_rect);
                clipped.text(pos2(label_rect.left(), rect.center().y), Align2::LEFT_CENTER, &s.label, FontId::proportional(13.0), ink);
                let values = [s.value.unwrap_or(f32::NAN), s.min, s.max, s.avg()];
                let mut x = rect.right() - if show_trend { TREND_W + 12.0 } else { 0.0 };
                for (i, (_, on)) in cols.iter().enumerate().rev() {
                    if *on {
                        let color = if i == 0 { ink } else { p.muted };
                        painter.text(
                            pos2(x - 8.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            vs.fmt(kind, values[i]),
                            FontId::monospace(12.5),
                            color,
                        );
                        x -= NUM_W;
                    }
                }
                if show_trend {
                    let spark = Rect::from_min_size(pos2(rect.right() - TREND_W, rect.top() + 3.0), vec2(TREND_W, ROW - 6.0));
                    let vals: Vec<f32> = draw::tail(&s.history, 120).collect();
                    let (span, floor, ceil) = draw::kind_range(kind);
                    let (lo, hi) = draw::value_range(vals.iter().copied(), span, floor, ceil);
                    let color = if kind == Kind::Temperature && let Some(v) = s.value {
                        draw::temp_status(v, &draw::thresholds(d.class, &d.detail, s)).color(&p)
                    } else {
                        p.accent
                    };
                    draw::sparkline(painter, spark, &vals, lo, hi, color, 120);
                }
                if resp.on_hover_text("Click to chart. Ctrl-click to compare.").clicked() {
                    let add = ui.input(|i| i.modifiers.command);
                    vs.toggle_focus(snap, &d.key, &s.key, add);
                }
            }
        }
        ui.add_space(6.0);
    }
}
