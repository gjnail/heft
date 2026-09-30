//! Drawing for the Hardware page: colors, temperature status, gauges,
//! meters, sparklines and the history chart.

use std::collections::VecDeque;
use std::f32::consts::PI;

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, Response, Sense, Shape, Stroke};

use crate::sensors::{Class, Kind, Sensor};

/// Chart colors. Status colors are reserved for good/warm/hot and always
/// come with a word; series colors identify lines in a chart.
pub struct Palette {
    pub good: Color32,
    pub warning: Color32,
    pub serious: Color32,
    pub critical: Color32,
    pub accent: Color32,
    pub series: [Color32; 4],
    pub grid: Color32,
    pub track: Color32,
    pub ink: Color32,
    pub muted: Color32,
}

pub fn palette(ui: &egui::Ui) -> Palette {
    let v = ui.visuals();
    let dark = v.dark_mode;
    let hex = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
    Palette {
        good: hex(0x0ca30c),
        warning: hex(0xfab219),
        serious: hex(0xec835a),
        critical: hex(0xd03b3b),
        accent: if dark { hex(0x3987e5) } else { hex(0x2a78d6) },
        series: if dark {
            [hex(0x3987e5), hex(0xd95926), hex(0x199e70), hex(0xc98500)]
        } else {
            [hex(0x2a78d6), hex(0xeb6834), hex(0x1baf7a), hex(0xeda100)]
        },
        grid: if dark { hex(0x2c2c2a) } else { hex(0xe1e0d9) },
        track: if dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(16) },
        ink: v.text_color(),
        muted: v.weak_text_color(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Status {
    Normal,
    Warm,
    Hot,
    Critical,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Normal => "Normal",
            Status::Warm => "Warm",
            Status::Hot => "Hot",
            Status::Critical => "Too hot",
        }
    }

    pub fn color(self, p: &Palette) -> Color32 {
        match self {
            Status::Normal => p.good,
            Status::Warm => p.warning,
            Status::Hot => p.serious,
            Status::Critical => p.critical,
        }
    }
}

/// Where a temperature starts counting as warm, hot and too hot, and why.
pub struct Thresholds {
    pub warm: f32,
    pub hot: f32,
    pub critical: f32,
    pub why: &'static str,
}

pub fn thresholds(class: Class, detail: &str, s: &Sensor) -> Thresholds {
    if let Some(l) = s.limits
        && let Some(warn) = l.warn
    {
        let critical = l.crit.unwrap_or(warn + 10.0);
        return Thresholds {
            warm: warn - 10.0,
            hot: warn,
            critical,
            why: "Hot and too hot are the limits the device itself reports.",
        };
    }
    let (warm, hot, critical, why) = match class {
        Class::Cpu => (
            75.0,
            85.0,
            95.0,
            "Rule of thumb for desktop processors. Many are designed to run near 90 °C under full load, so warm or hot while busy is normal; the same while idle suggests a cooling problem.",
        ),
        Class::Gpu => (
            75.0,
            83.0,
            90.0,
            "Rule of thumb for graphics cards. Most slow themselves down somewhere between 83 and 93 °C.",
        ),
        Class::Storage if detail.contains("Hard drive") => {
            (45.0, 50.0, 55.0, "Hard drives last longest below about 45 °C; most are rated to 55-60 °C.")
        }
        Class::Storage => (
            55.0,
            65.0,
            75.0,
            "Rule of thumb for SSDs. Most start slowing down to protect themselves around 70-80 °C.",
        ),
        Class::Battery => (40.0, 45.0, 55.0, "Batteries age faster above about 40 °C."),
        _ => (60.0, 75.0, 90.0, "Rule of thumb for motherboard and other sensors."),
    };
    Thresholds { warm, hot, critical, why }
}

pub fn temp_status(t: f32, th: &Thresholds) -> Status {
    if t >= th.critical {
        Status::Critical
    } else if t >= th.hot {
        Status::Hot
    } else if t >= th.warm {
        Status::Warm
    } else {
        Status::Normal
    }
}

/// A small colored dot and the status word; hover explains the thresholds.
pub fn status_chip(ui: &mut egui::Ui, status: Status, th: &Thresholds, kind_fmt: impl Fn(f32) -> String) -> Response {
    let p = palette(ui);
    let r = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, status.color(&p));
            ui.label(egui::RichText::new(status.label()).size(12.0).color(p.muted));
        })
        .response;
    r.on_hover_ui(|ui| {
        ui.set_max_width(300.0);
        ui.label(format!(
            "Warm from {}, hot from {}, too hot from {}.",
            kind_fmt(th.warm),
            kind_fmt(th.hot),
            kind_fmt(th.critical)
        ));
        ui.label(egui::RichText::new(th.why).weak());
    })
}

/// The recent end of a history, for sparklines.
pub fn tail(h: &VecDeque<f32>, n: usize) -> impl Iterator<Item = f32> + '_ {
    h.iter().skip(h.len().saturating_sub(n)).copied()
}

/// Range to scale a line to: the data's own range, widened to at least
/// `min_span`, and never below `floor` when given (percentages start at 0).
pub fn value_range(values: impl Iterator<Item = f32>, min_span: f32, floor: Option<f32>, ceil: Option<f32>) -> (f32, f32) {
    let (mut lo, mut hi) = values.filter(|v| v.is_finite()).fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
    if lo > hi {
        return (floor.unwrap_or(0.0), floor.unwrap_or(0.0) + min_span.max(1.0));
    }
    if hi - lo < min_span {
        let mid = (hi + lo) / 2.0;
        lo = mid - min_span / 2.0;
        hi = mid + min_span / 2.0;
    }
    if let Some(f) = floor
        && lo < f
    {
        hi += f - lo;
        lo = f;
    }
    if let Some(c) = ceil
        && hi > c
    {
        lo = (lo - (hi - c)).max(floor.unwrap_or(f32::MIN));
        hi = c;
    }
    (lo, hi)
}

/// Scale settings per kind: minimum visible span and fixed ends.
pub fn kind_range(kind: Kind) -> (f32, Option<f32>, Option<f32>) {
    match kind {
        Kind::Load | Kind::Duty | Kind::Level => (100.0, Some(0.0), Some(100.0)),
        Kind::Temperature => (10.0, None, None),
        Kind::Clock => (500.0, Some(0.0), None),
        Kind::Voltage => (0.05, None, None),
        Kind::Fan => (300.0, Some(0.0), None),
        Kind::Rate => (1024.0 * 64.0, Some(0.0), None),
        Kind::Power => (10.0, Some(0.0), None),
        _ => (1.0, None, None),
    }
}

/// A line with a faint wash under it. Gaps (NaN) break the line.
pub fn sparkline(painter: &egui::Painter, rect: Rect, values: &[f32], lo: f32, hi: f32, color: Color32, capacity: usize) {
    // A faint baseline across the whole width, so the part with no data yet
    // reads as "not measured yet" rather than a glitch.
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, color.gamma_multiply(0.25)));
    if values.is_empty() || hi <= lo {
        return;
    }
    let n = capacity.max(values.len()).max(2);
    let dx = rect.width() / (n - 1) as f32;
    let x0 = rect.right() - (values.len() - 1) as f32 * dx;
    let y = |v: f32| rect.bottom() - ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * rect.height();
    let mut run: Vec<Pos2> = Vec::new();
    let flush = |run: &mut Vec<Pos2>| {
        if run.len() >= 2 {
            let mut mesh = egui::Mesh::default();
            let wash = color.gamma_multiply(0.14);
            for w in run.windows(2) {
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(w[0], wash);
                mesh.colored_vertex(w[1], wash);
                mesh.colored_vertex(pos2(w[1].x, rect.bottom()), wash);
                mesh.colored_vertex(pos2(w[0].x, rect.bottom()), wash);
                mesh.add_triangle(i, i + 1, i + 2);
                mesh.add_triangle(i, i + 2, i + 3);
            }
            painter.add(Shape::mesh(mesh));
            painter.add(Shape::line(std::mem::take(run), Stroke::new(1.5, color)));
        } else {
            run.clear();
        }
    };
    for (i, v) in values.iter().enumerate() {
        if v.is_finite() {
            run.push(pos2(x0 + i as f32 * dx, y(*v)));
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);
}


/// A 240-degree ring gauge with the value in the middle.
pub fn gauge(ui: &mut egui::Ui, size: f32, frac: Option<f32>, color: Color32, value: &str, unit: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size * 0.86), Sense::click());
    let p = palette(ui);
    let painter = ui.painter_at(rect.expand(2.0));
    let c = pos2(rect.center().x, rect.top() + size / 2.0);
    let r = size / 2.0 - 7.0;
    let width = (size * 0.075).clamp(6.0, 11.0);
    let start = PI * 0.75; // 7:30 on a clock face, sweeping clockwise to 4:30
    let sweep = PI * 1.5;
    let arc = |from: f32, to: f32| -> Vec<Pos2> {
        let steps = ((to - from).abs() / sweep * 64.0).ceil().max(2.0) as usize;
        (0..=steps)
            .map(|i| {
                let a = start + sweep * (from + (to - from) * i as f32 / steps as f32);
                c + vec2(a.cos(), a.sin()) * r
            })
            .collect()
    };
    let cap = |f: f32, col: Color32| {
        let a = start + sweep * f;
        painter.circle_filled(c + vec2(a.cos(), a.sin()) * r, width / 2.0, col);
    };
    painter.add(Shape::line(arc(0.0, 1.0), Stroke::new(width, p.track)));
    cap(0.0, p.track);
    cap(1.0, p.track);
    if let Some(f) = frac {
        let f = f.clamp(0.0, 1.0);
        if f > 0.005 {
            painter.add(Shape::line(arc(0.0, f), Stroke::new(width, color)));
            cap(0.0, color);
            cap(f, color);
        }
    }
    let big = (size * 0.2).clamp(16.0, 30.0);
    painter.text(c + vec2(0.0, -big * 0.1), Align2::CENTER_CENTER, value, FontId::proportional(big), p.ink);
    painter.text(c + vec2(0.0, big * 0.72), Align2::CENTER_CENTER, unit, FontId::proportional(12.0), p.muted);
    resp
}

/// One line in a history chart.
pub struct Series<'a> {
    pub label: String,
    pub color: Color32,
    pub sensor: &'a Sensor,
}

/// A time chart of one or more sensors of the same kind (one axis), with
/// gridlines, a crosshair and a readout under the pointer.
pub fn history_chart(
    ui: &mut egui::Ui,
    height: f32,
    times: &VecDeque<f64>,
    series: &[Series],
    kind: Kind,
    window_secs: f64,
    fahrenheit: bool,
) {
    let p = palette(ui);
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter_at(rect);
    let Some(&now) = times.back() else { return };
    let t0 = now - window_secs;
    let first = times.iter().position(|&t| t >= t0).unwrap_or(times.len());

    let font = FontId::proportional(11.0);
    let axis_w = 58.0;
    let plot = Rect::from_min_max(pos2(rect.left() + axis_w, rect.top() + 6.0), pos2(rect.right() - 8.0, rect.bottom() - 18.0));
    let visible = |s: &Series| s.sensor.history.iter().skip(first).copied().collect::<Vec<f32>>();
    let (span, floor, ceil) = kind_range(kind);
    let all: Vec<f32> = series.iter().flat_map(visible).map(|v| kind.display(v, fahrenheit)).collect();
    let (lo, hi) = value_range(all.iter().copied(), span, floor, ceil);
    let (lo, hi, step) = nice_ticks(lo, hi, 4);
    let y = |v: f32| plot.bottom() - ((kind.display(v, fahrenheit) - lo) / (hi - lo)).clamp(0.0, 1.0) * plot.height();
    let x = |t: f64| plot.left() + ((t - t0) / window_secs) as f32 * plot.width();

    // Gridlines and value ticks.
    let mut v = lo;
    while v <= hi + step * 0.01 {
        let yy = plot.bottom() - (v - lo) / (hi - lo) * plot.height();
        painter.line_segment([pos2(plot.left(), yy), pos2(plot.right(), yy)], Stroke::new(1.0, p.grid));
        painter.text(pos2(plot.left() - 8.0, yy), Align2::RIGHT_CENTER, axis_label(kind, v, step), font.clone(), p.muted);
        v += step;
    }
    // Time ticks every minute (or 15 s for short windows), counted back from now.
    let tick = if window_secs <= 60.0 { 15.0 } else if window_secs <= 300.0 { 60.0 } else { 120.0 };
    let mut ago = 0.0;
    while ago <= window_secs + 0.1 {
        let xx = x(now - ago);
        let label = if ago == 0.0 { "now".to_string() } else if ago < 60.0 { format!("{ago:.0}s") } else { format!("{:.0}m", ago / 60.0) };
        painter.text(pos2(xx, plot.bottom() + 4.0), Align2::CENTER_TOP, label, font.clone(), p.muted);
        ago += tick;
    }

    for s in series {
        let mut run: Vec<Pos2> = Vec::new();
        for (i, v) in s.sensor.history.iter().enumerate().skip(first) {
            if v.is_finite() {
                run.push(pos2(x(times[i]), y(*v)));
            } else if run.len() > 1 {
                painter.add(Shape::line(std::mem::take(&mut run), Stroke::new(2.0, s.color)));
            } else {
                run.clear();
            }
        }
        if series.len() == 1 && run.len() > 1 {
            let mut mesh = egui::Mesh::default();
            let wash = s.color.gamma_multiply(0.12);
            for w in run.windows(2) {
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(w[0], wash);
                mesh.colored_vertex(w[1], wash);
                mesh.colored_vertex(pos2(w[1].x, plot.bottom()), wash);
                mesh.colored_vertex(pos2(w[0].x, plot.bottom()), wash);
                mesh.add_triangle(i, i + 1, i + 2);
                mesh.add_triangle(i, i + 2, i + 3);
            }
            painter.add(Shape::mesh(mesh));
        }
        if run.len() > 1 {
            painter.add(Shape::line(run, Stroke::new(2.0, s.color)));
        }
    }

    // Crosshair and readout for the sample nearest the pointer.
    if let Some(pos) = resp.hover_pos()
        && plot.x_range().contains(pos.x)
    {
        let t = t0 + ((pos.x - plot.left()) / plot.width()) as f64 * window_secs;
        let i = (first..times.len()).min_by(|&a, &b| (times[a] - t).abs().total_cmp(&(times[b] - t).abs()));
        if let Some(i) = i {
            let xx = x(times[i]);
            painter.line_segment([pos2(xx, plot.top()), pos2(xx, plot.bottom())], Stroke::new(1.0, p.muted));
            for s in series {
                if let Some(v) = s.sensor.history.get(i).filter(|v| v.is_finite()) {
                    let c = pos2(xx, y(*v));
                    painter.circle_filled(c, 6.0, ui.visuals().extreme_bg_color);
                    painter.circle_filled(c, 4.0, s.color);
                }
            }
            let ago = now - times[i];
            resp.on_hover_ui_at_pointer(|ui| {
                ui.label(egui::RichText::new(if ago < 1.0 { "Now".into() } else { format!("{ago:.0} seconds ago") }).weak());
                for s in series {
                    let v = s.sensor.history.get(i).copied().unwrap_or(f32::NAN);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                        ui.painter().rect_filled(r, 2.0, s.color);
                        ui.label(&s.label);
                        ui.label(egui::RichText::new(kind.format(v, fahrenheit)).strong());
                    });
                }
            });
        }
    }
}

/// Tick labels with as many decimals as the step needs (2.5 shows as 2.5).
fn axis_label(kind: Kind, v: f32, step: f32) -> String {
    let decimals = if step.fract().abs() < 1e-4 { 0 } else if (step * 10.0).fract().abs() < 1e-3 { 1 } else { 2 };
    match kind {
        Kind::Rate => format!("{}/s", crate::util::fmt_size(v.max(0.0) as u64)),
        Kind::Data => crate::util::fmt_size(v.max(0.0) as u64),
        _ if decimals > 0 => format!("{v:.decimals$}"),
        _ if v.abs() >= 1000.0 => crate::util::fmt_count(v.round().max(0.0) as u64),
        _ => format!("{v:.0}"),
    }
}

/// Round axis ends and a tick step that lands on clean numbers.
pub fn nice_ticks(lo: f32, hi: f32, count: u32) -> (f32, f32, f32) {
    let span = (hi - lo).max(1e-6);
    let raw = span / count as f32;
    let mag = 10f32.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    let lo = (lo / step).floor() * step;
    let hi = (hi / step).ceil() * step;
    (lo, hi, step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks() {
        assert_eq!(nice_ticks(41.0, 58.0, 4), (40.0, 60.0, 5.0));
        assert_eq!(nice_ticks(0.0, 100.0, 4), (0.0, 100.0, 25.0));
    }

    #[test]
    fn ranges() {
        assert_eq!(value_range([50.0, 52.0].into_iter(), 10.0, None, None), (46.0, 56.0));
        assert_eq!(value_range([3.0, 5.0].into_iter(), 100.0, Some(0.0), Some(100.0)), (0.0, 100.0));
    }
}
