//! The dashboard: headline gauges for what people check most, then a card
//! per device with its key readings. Every reading can be clicked to chart it.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Stroke};

use super::draw::{self, Palette};
use super::{short_name, ViewState};
use crate::sensors::{Class, Device, Driver, Kind, Sensor, Snapshot};

const GAP: f32 = 10.0;
/// Sparklines show the last two minutes at one sample per second.
const SPARK: usize = 120;

pub fn show(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState) {
    tiles(ui, snap, vs);
    ui.add_space(GAP);
    cards(ui, snap, vs);
}

// ----------------------------------------------------------------------
// Headline tiles

enum TileValue<'a> {
    Reading(&'a Device, &'a Sensor),
    /// Not available, and why.
    Missing(&'static str),
}

struct Tile<'a> {
    title: String,
    value: TileValue<'a>,
    sub: String,
}

fn cpu_temp(d: &Device) -> Option<&Sensor> {
    d.first_of(Kind::Temperature, &["Tctl/Tdie", "Tdie", "Package", "Core max", "Tctl"]).or_else(|| d.of_kind(Kind::Temperature).next())
}

fn primary_temp(d: &Device) -> Option<&Sensor> {
    match d.class {
        Class::Cpu => cpu_temp(d),
        Class::Gpu => d.first_of(Kind::Temperature, &["Core"]),
        Class::Storage => d.first_of(Kind::Temperature, &["Drive"]),
        Class::Motherboard => d.first_of(Kind::Temperature, &["Motherboard", "System (SYSTIN)"]),
        _ => None,
    }
    .or_else(|| d.of_kind(Kind::Temperature).next())
}

/// The graphics card people mean: a discrete one if there is one.
fn main_gpu(snap: &Snapshot) -> Option<&Device> {
    let gpus: Vec<&Device> = snap.devices.iter().filter(|d| d.class == Class::Gpu).collect();
    gpus.iter().find(|d| d.detail != "Integrated").or(gpus.first()).copied()
}

fn tiles(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState) {
    let mut tiles: Vec<Tile> = Vec::new();
    let find = |c: Class| snap.devices.iter().find(|d| d.class == c);

    if let Some(cpu) = find(Class::Cpu) {
        let why = match snap.driver {
            Driver::NotInstalled => "Needs the PawnIO driver",
            Driver::NeedsAdmin => "Needs administrator",
            _ => "Not available for this processor",
        };
        let mut sub = Vec::new();
        if let Some(p) = cpu.first_of(Kind::Power, &["Package"]).and_then(|s| s.value) {
            sub.push(format!("{} package", vs.fmt(Kind::Power, p)));
        }
        tiles.push(Tile {
            title: "CPU temperature".into(),
            value: match cpu_temp(cpu) {
                Some(s) => TileValue::Reading(cpu, s),
                None => TileValue::Missing(why),
            },
            sub: sub.join(" · "),
        });
        if let Some(load) = cpu.first_of(Kind::Load, &["Total"]) {
            let clock = cpu.first_of(Kind::Clock, &["Average"]).and_then(|s| s.value);
            tiles.push(Tile {
                title: "CPU load".into(),
                value: TileValue::Reading(cpu, load),
                sub: clock.map(|c| format!("{:.2} GHz average", c / 1000.0)).unwrap_or_default(),
            });
        }
    }
    if let Some(gpu) = main_gpu(snap) {
        let name = short_name(&gpu.name).replace("NVIDIA ", "").replace("GeForce ", "").replace("AMD ", "");
        if let Some(t) = primary_temp(gpu) {
            let fan = gpu.of_kind(Kind::Fan).next().and_then(|s| s.value);
            tiles.push(Tile {
                title: "GPU temperature".into(),
                value: TileValue::Reading(gpu, t),
                sub: fan.map(|f| format!("{name} · fan {}", vs.fmt(Kind::Fan, f))).unwrap_or(name.clone()),
            });
        }
        if let Some(load) = gpu.first_of(Kind::Load, &["Core"]) {
            let power = gpu.first_of(Kind::Power, &["Board"]).and_then(|s| s.value);
            tiles.push(Tile {
                title: "GPU load".into(),
                value: TileValue::Reading(gpu, load),
                sub: power.map(|w| format!("{} board power", vs.fmt(Kind::Power, w))).unwrap_or_default(),
            });
        }
    }
    if let Some(mem) = find(Class::Memory)
        && let Some(load) = mem.first_of(Kind::Load, &["Physical"])
    {
        let used = mem.first_of(Kind::Data, &["Used"]).and_then(|s| s.value);
        let avail = mem.first_of(Kind::Data, &["Available"]).and_then(|s| s.value);
        let sub = match (used, avail) {
            (Some(u), Some(a)) => format!("{} of {}", crate::util::fmt_size(u as u64), crate::util::fmt_size((u + a) as u64)),
            _ => String::new(),
        };
        tiles.push(Tile { title: "Memory in use".into(), value: TileValue::Reading(mem, load), sub });
    }
    let hottest = snap
        .devices
        .iter()
        .filter(|d| d.class == Class::Storage)
        .filter_map(|d| Some((d, primary_temp(d)?)))
        .filter(|(_, s)| s.value.is_some())
        .max_by(|a, b| a.1.value.unwrap_or(0.0).total_cmp(&b.1.value.unwrap_or(0.0)));
    if let Some((d, s)) = hottest {
        tiles.push(Tile { title: "Hottest drive".into(), value: TileValue::Reading(d, s), sub: short_name(&d.name) });
    }

    let width = ui.available_width();
    let cols = ((width + GAP) / (190.0 + GAP)).floor().clamp(1.0, tiles.len().max(1) as f32) as usize;
    let tile_w = (width - GAP * (cols - 1) as f32) / cols as f32;
    for row in tiles.chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for t in row {
                ui.allocate_ui_with_layout(vec2(tile_w, 0.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                    tile(ui, t, tile_w, vs, snap);
                });
            }
        });
        ui.add_space(GAP);
    }
}

fn frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(12))
}

fn tile(ui: &mut egui::Ui, t: &Tile, width: f32, vs: &mut ViewState, snap: &Snapshot) {
    let p = draw::palette(ui);
    frame(ui)
        .show(ui, |ui| {
            ui.set_width(width - 26.0);
            ui.set_height(182.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&t.title).size(13.0).color(p.muted));
                if let TileValue::Reading(d, s) = t.value
                    && s.kind == Kind::Temperature
                    && let Some(v) = s.value
                {
                    let th = draw::thresholds(d.class, &d.detail, s);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        draw::status_chip(ui, draw::temp_status(v, &th), &th, |x| vs.fmt(Kind::Temperature, x));
                    });
                }
            });
            match t.value {
                TileValue::Reading(d, s) => {
                    let v = s.value.unwrap_or(f32::NAN);
                    let (frac, color) = gauge_fill(d, s, v, &p);
                    let number = s.kind.format_number(v, vs.fahrenheit);
                    let unit = s.kind.unit(vs.fahrenheit);
                    let g = draw::gauge(ui, 118.0, frac, color, &number, unit);
                    let g = g.on_hover_text(format!(
                        "{}\nmin {}   avg {}   max {}\nClick to chart",
                        s.label,
                        vs.fmt(s.kind, s.min),
                        vs.fmt(s.kind, s.avg()),
                        vs.fmt(s.kind, s.max)
                    ));
                    if g.clicked() {
                        let add = ui.input(|i| i.modifiers.command);
                        vs.toggle_focus(snap, &d.key, &s.key, add);
                    }
                    ui.label(RichText::new(&t.sub).size(12.0).color(p.muted));
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
                    let vals: Vec<f32> = draw::tail(&s.history, SPARK).collect();
                    let (span, floor, ceil) = draw::kind_range(s.kind);
                    let (lo, hi) = draw::value_range(vals.iter().copied(), span, floor, ceil);
                    draw::sparkline(ui.painter(), rect, &vals, lo, hi, p.accent, SPARK);
                }
                TileValue::Missing(why) => {
                    ui.add_space(8.0);
                    draw::gauge(ui, 118.0, None, p.track, "-", "");
                    ui.label(RichText::new(why).size(12.0).color(p.muted));
                }
            }
        });
}

/// How full a gauge is and in what color: temperatures by status, the
/// rest in the accent color (a busy CPU isn't a problem).
fn gauge_fill(d: &Device, s: &Sensor, v: f32, p: &Palette) -> (Option<f32>, Color32) {
    if !v.is_finite() {
        return (None, p.track);
    }
    match s.kind {
        Kind::Temperature => {
            let th = draw::thresholds(d.class, &d.detail, s);
            let lo = 20.0;
            let hi = th.critical + 5.0;
            (Some((v - lo) / (hi - lo)), draw::temp_status(v, &th).color(p))
        }
        Kind::Load | Kind::Level | Kind::Duty => (Some(v / 100.0), p.accent),
        _ => (Some(0.0), p.accent),
    }
}

// ----------------------------------------------------------------------
// Device cards

fn cards(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState) {
    let width = ui.available_width();
    let cols = ((width + GAP) / (330.0 + GAP)).floor().clamp(1.0, 3.0) as usize;
    let card_w = (width - GAP * (cols - 1) as f32) / cols as f32;
    // Fill columns top to bottom, each device going to the shortest column,
    // so cards of different heights pack without gaps.
    let devices: Vec<&Device> = snap.devices.iter().filter(|d| !d.sensors.is_empty()).collect();
    let mut columns: Vec<Vec<&Device>> = vec![Vec::new(); cols];
    let mut heights = vec![0.0f32; cols];
    for d in devices {
        let i = (0..cols).min_by(|&a, &b| heights[a].total_cmp(&heights[b])).unwrap_or(0);
        heights[i] += estimate_height(d);
        columns[i].push(d);
    }
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        for col in &columns {
            ui.allocate_ui_with_layout(vec2(card_w, 0.0), egui::Layout::top_down(egui::Align::LEFT), |ui| {
                ui.spacing_mut().item_spacing.y = GAP;
                for d in col {
                    card(ui, d, card_w, snap, vs);
                }
            });
        }
    });
}

fn estimate_height(d: &Device) -> f32 {
    let rows = match d.class {
        Class::Cpu => 6,
        Class::Motherboard => d.sensors.len().min(24),
        _ => d.sensors.len().min(10),
    };
    60.0 + rows as f32 * 24.0
}

/// What a row draws between its label and its value.
enum Viz<'a> {
    None,
    Meter(f32, Color32),
    Spark(&'a Sensor, Color32),
}

struct Rows<'a, 'b> {
    ui: &'a mut egui::Ui,
    dev: &'b Device,
    snap: &'b Snapshot,
    vs: &'a mut ViewState,
    p: Palette,
    width: f32,
}

impl<'b> Rows<'_, 'b> {
    /// One reading: label, a meter or sparkline, the value. Click to chart.
    fn row(&mut self, label: &str, s: &'b Sensor, viz: Viz) {
        let (rect, resp) = self.ui.allocate_exact_size(vec2(self.width, 22.0), Sense::click());
        let focused = self.vs.focus.iter().any(|(d, k)| *d == self.dev.key && *k == s.key);
        if focused {
            self.ui.painter().rect_filled(rect.expand2(vec2(4.0, 0.0)), 4.0, self.ui.visuals().selection.bg_fill.gamma_multiply(0.5));
        } else if resp.hovered() {
            self.ui.painter().rect_filled(rect.expand2(vec2(4.0, 0.0)), 4.0, self.ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let painter = self.ui.painter();
        let font = FontId::proportional(13.0);
        let stale = s.value.is_none();
        let ink = if stale { self.p.muted } else { self.p.ink };
        let label_w = (self.width * 0.42).min(150.0);
        let value_w = 92.0;
        painter.text(pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, label, font.clone(), self.p.muted);
        let v = s.value.unwrap_or(f32::NAN);
        painter.text(pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, self.vs.fmt(s.kind, v), font, ink);
        let mid = Rect::from_min_max(
            pos2(rect.left() + label_w, rect.top() + 3.0),
            pos2(rect.right() - value_w - 6.0, rect.bottom() - 3.0),
        );
        if mid.width() > 16.0 {
            match viz {
                Viz::None => {}
                Viz::Meter(frac, color) => {
                    let bar = Rect::from_center_size(mid.center(), vec2(mid.width(), 6.0));
                    painter.rect_filled(bar, 3.0, self.p.track);
                    let mut f = bar;
                    f.set_width(bar.width() * frac.clamp(0.0, 1.0));
                    if f.width() > 0.5 {
                        painter.rect_filled(f, 3.0, color);
                    }
                }
                Viz::Spark(sensor, color) => {
                    let vals: Vec<f32> = draw::tail(&sensor.history, SPARK).collect();
                    let (span, floor, ceil) = draw::kind_range(sensor.kind);
                    let (lo, hi) = draw::value_range(vals.iter().copied(), span, floor, ceil);
                    draw::sparkline(painter, mid, &vals, lo, hi, color, SPARK);
                }
            }
        }
        let kind = s.kind;
        let resp = resp.on_hover_ui(|ui| {
            ui.label(RichText::new(format!("{} · {}", short_name(&self.dev.name), s.label)).strong());
            ui.label(format!(
                "min {}   avg {}   max {}",
                self.vs.fmt(kind, s.min),
                self.vs.fmt(kind, s.avg()),
                self.vs.fmt(kind, s.max)
            ));
            ui.label(RichText::new("Click to chart. Ctrl-click to compare.").weak());
        });
        if resp.clicked() {
            let add = self.ui.input(|i| i.modifiers.command);
            self.vs.toggle_focus(self.snap, &self.dev.key, &s.key, add);
        }
    }

    fn temp(&mut self, label: &str, s: &'b Sensor) {
        let v = s.value.unwrap_or(f32::NAN);
        let th = draw::thresholds(self.dev.class, &self.dev.detail, s);
        let color = if v.is_finite() { draw::temp_status(v, &th).color(&self.p) } else { self.p.track };
        self.row(label, s, Viz::Spark(s, color));
    }

    fn load(&mut self, label: &str, s: &'b Sensor) {
        let v = s.value.unwrap_or(0.0);
        let accent = self.p.accent;
        self.row(label, s, Viz::Meter(v / 100.0, accent));
    }

    fn plain(&mut self, label: &str, s: &'b Sensor) {
        self.row(label, s, Viz::None);
    }

    fn spark(&mut self, label: &str, s: &'b Sensor, color: Color32) {
        self.row(label, s, Viz::Spark(s, color));
    }

    fn section(&mut self, title: &str) {
        self.ui.add_space(4.0);
        self.ui.label(RichText::new(title).size(11.5).color(self.p.muted).strong());
    }
}

fn card(ui: &mut egui::Ui, d: &Device, width: f32, snap: &Snapshot, vs: &mut ViewState) {
    frame(ui).show(ui, |ui| {
        let inner = width - 26.0;
        ui.set_width(inner);
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(short_name(&d.name)).strong().size(14.0)).truncate());
        });
        let sub = if d.detail.is_empty() { d.class.label().to_string() } else { format!("{} · {}", d.class.label(), d.detail) };
        ui.label(RichText::new(sub).size(11.5).weak());
        ui.add_space(6.0);
        let p = draw::palette(ui);
        let mut r = Rows { ui, dev: d, snap, vs, p, width: inner };
        match d.class {
            Class::Cpu => cpu_card(&mut r),
            Class::Gpu => gpu_card(&mut r),
            Class::Memory => memory_card(&mut r),
            Class::Storage => drive_card(&mut r),
            Class::Network => network_card(&mut r),
            Class::Motherboard => board_card(&mut r),
            _ => generic_card(&mut r),
        }
    });
}

fn cpu_card(r: &mut Rows) {
    let d = r.dev;
    for s in d.of_kind(Kind::Temperature).filter(|s| !s.label.starts_with("Core #")) {
        r.temp(&s.label, s);
    }
    if let Some(s) = d.first_of(Kind::Load, &["Total"]) {
        r.load("Total load", s);
    }
    if let Some(s) = d.first_of(Kind::Clock, &["Average"]) {
        let accent = r.p.accent;
        r.spark("Average clock", s, accent);
    }
    for s in d.of_kind(Kind::Power) {
        r.plain(&format!("{} power", s.label), s);
    }
    core_strip(r);
}

/// One bar per core: how busy it is, with its clock on hover.
fn core_strip(r: &mut Rows) {
    let d = r.dev;
    let cores: Vec<&Sensor> = d.of_kind(Kind::Load).filter(|s| s.label.starts_with("Core #")).collect();
    if cores.is_empty() {
        return;
    }
    r.section("Cores");
    let n = cores.len();
    let gap = if n > 24 { 1.0 } else { 3.0 };
    let (rect, resp) = r.ui.allocate_exact_size(vec2(r.width, 46.0), Sense::click());
    let bar_w = ((rect.width() - gap * (n - 1) as f32) / n as f32).max(1.0);
    let painter = r.ui.painter();
    let mut hovered = None;
    for (i, s) in cores.iter().enumerate() {
        let x = rect.left() + i as f32 * (bar_w + gap);
        let col = Rect::from_min_size(pos2(x, rect.top()), vec2(bar_w, rect.height()));
        painter.rect_filled(col, 3.0, r.p.track);
        let v = s.value.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
        if v > 0.0 {
            let fill = Rect::from_min_max(pos2(col.left(), col.bottom() - col.height() * v), col.max);
            painter.rect_filled(fill, 3.0, r.p.accent);
        }
        if resp.hover_pos().is_some_and(|p| col.expand2(vec2(gap / 2.0, 0.0)).contains(p)) {
            hovered = Some((i, *s));
            painter.rect_stroke(col, 3.0, Stroke::new(1.0, r.p.ink), egui::StrokeKind::Outside);
        }
    }
    if let Some((_, s)) = hovered {
        let clock = d.sensor(Kind::Clock, &s.label).and_then(|c| c.value);
        let temp = d.sensor(Kind::Temperature, &s.label).and_then(|c| c.value);
        let resp = resp.clone().on_hover_ui_at_pointer(|ui| {
            ui.label(RichText::new(&s.label).strong());
            ui.label(format!("{} busy", r.vs.fmt(Kind::Load, s.value.unwrap_or(f32::NAN))));
            if let Some(c) = clock {
                ui.label(r.vs.fmt(Kind::Clock, c));
            }
            if let Some(t) = temp {
                ui.label(r.vs.fmt(Kind::Temperature, t));
            }
        });
        if resp.clicked() {
            let add = r.ui.input(|i| i.modifiers.command);
            r.vs.toggle_focus(r.snap, &d.key, &s.key, add);
        }
    }
}

fn gpu_card(r: &mut Rows) {
    let d = r.dev;
    for s in d.of_kind(Kind::Temperature) {
        r.temp(&format!("{} temperature", s.label), s);
    }
    if let Some(s) = d.first_of(Kind::Load, &["Core"]) {
        r.load("Load", s);
    }
    if let Some(s) = d.first_of(Kind::Level, &["Memory"]) {
        let used = d.first_of(Kind::Data, &["Memory used"]).and_then(|u| u.value);
        let label = match used {
            Some(u) => format!("Memory · {}", crate::util::fmt_size(u as u64)),
            None => "Memory".into(),
        };
        r.load(&label, s);
    } else if let Some(s) = d.first_of(Kind::Data, &["Shared memory used", "Memory used"]) {
        r.plain("Memory used", s);
    }
    if let Some(s) = d.first_of(Kind::Power, &["Board"]) {
        let accent = r.p.accent;
        r.spark("Power", s, accent);
    } else if let Some(s) = d.first_of(Kind::Level, &["Board power"]) {
        r.load("Power (of limit)", s);
    }
    for s in d.of_kind(Kind::Clock) {
        r.plain(&format!("{} clock", s.label), s);
    }
    for s in d.of_kind(Kind::Fan) {
        r.plain(&s.label, s);
    }
    for s in d.of_kind(Kind::Duty) {
        let v = s.value.unwrap_or(0.0);
        let accent = r.p.accent;
        r.row(&format!("{} drive", s.label), s, Viz::Meter(v / 100.0, accent));
    }
    let engines: Vec<&Sensor> = d
        .of_kind(Kind::Load)
        .filter(|s| matches!(s.label.as_str(), "Video decode" | "Video encode" | "Compute") && s.max > 0.5)
        .collect();
    for s in engines {
        r.load(&s.label, s);
    }
}

fn memory_card(r: &mut Rows) {
    let d = r.dev;
    if let Some(s) = d.first_of(Kind::Load, &["Physical"]) {
        r.load("In use", s);
    }
    if let Some(s) = d.first_of(Kind::Data, &["Available"]) {
        r.plain("Available", s);
    }
    if let Some(s) = d.first_of(Kind::Load, &["Committed"]) {
        r.load("Committed", s);
    }
}

fn drive_card(r: &mut Rows) {
    let d = r.dev;
    for s in d.of_kind(Kind::Temperature) {
        let label = if s.label == "Drive" { "Temperature".to_string() } else { s.label.clone() };
        r.temp(&label, s);
    }
    if let Some(s) = d.first_of(Kind::Load, &["Activity"]) {
        r.load("Activity", s);
    }
    let (a, b) = (r.p.series[0], r.p.series[1]);
    if let Some(s) = d.first_of(Kind::Rate, &["Read"]) {
        r.spark("Read", s, a);
    }
    if let Some(s) = d.first_of(Kind::Rate, &["Write"]) {
        r.spark("Write", s, b);
    }
    if let Some(s) = d.first_of(Kind::Level, &["Space used"]) {
        r.load("Space used", s);
    }
}

fn network_card(r: &mut Rows) {
    let d = r.dev;
    let (a, b) = (r.p.series[0], r.p.series[1]);
    if let Some(s) = d.first_of(Kind::Rate, &["Download"]) {
        r.spark("Download", s, a);
    }
    if let Some(s) = d.first_of(Kind::Rate, &["Upload"]) {
        r.spark("Upload", s, b);
    }
}

fn board_card(r: &mut Rows) {
    let d = r.dev;
    let temps: Vec<&Sensor> = d.of_kind(Kind::Temperature).collect();
    if !temps.is_empty() {
        r.section("Temperatures");
        for s in temps {
            r.temp(&s.label, s);
        }
    }
    let fans: Vec<&Sensor> = d.of_kind(Kind::Fan).collect();
    if !fans.is_empty() {
        r.section("Fans");
        for s in fans {
            // The bar shows how hard the board is driving the fan.
            let duty = d.sensor(Kind::Duty, &s.label).and_then(|x| x.value).unwrap_or(0.0);
            let accent = r.p.accent;
            r.row(&s.label, s, Viz::Meter(duty / 100.0, accent));
        }
    }
    let volts: Vec<&Sensor> = d.of_kind(Kind::Voltage).collect();
    if !volts.is_empty() {
        r.section("Voltages");
        for s in volts {
            r.plain(&s.label, s);
        }
    }
}

fn generic_card(r: &mut Rows) {
    let d = r.dev;
    for kind in Kind::ALL {
        for s in d.of_kind(kind) {
            match kind {
                Kind::Temperature => r.temp(&s.label, s),
                Kind::Load | Kind::Level | Kind::Duty => r.load(&s.label, s),
                _ => r.plain(&s.label, s),
            }
        }
    }
}
