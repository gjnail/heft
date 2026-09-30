//! The Hardware workspace: live temperatures, loads, clocks, fans, voltages
//! and power, as a dashboard and as a full sensor table like HWMonitor's.

mod dashboard;
mod draw;
mod table;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui::{self, Align, Layout, RichText};

use crate::sensors::{Class, Driver, Kind, Monitor, NoteLevel, Snapshot};

/// Things the page asks the main app to do.
pub enum Event {
    Toast(String, bool),
    /// Restart elevated, coming back to this page.
    Elevate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Dashboard,
    Table,
}

/// Everything the page draws with besides the sensor data itself.
pub struct ViewState {
    view: View,
    fahrenheit: bool,
    /// How much history the big chart shows, in seconds.
    window: u32,
    /// Sensors in the big chart, as (device key, sensor key). All the same kind.
    focus: Vec<(String, String)>,
    collapsed: HashSet<String>,
    reset: bool,
    events: Vec<Event>,
}

impl ViewState {
    /// Click on a sensor: chart it. Ctrl-click adds it to the chart when it
    /// measures the same thing, so there's only ever one axis.
    fn toggle_focus(&mut self, snap: &Snapshot, dev: &str, sensor: &str, add: bool) {
        let key = (dev.to_string(), sensor.to_string());
        let kind_of = |(d, s): &(String, String)| snap.find(d, s).map(|(_, s)| s.kind);
        let same_kind = self.focus.first().and_then(kind_of) == kind_of(&key);
        if add && same_kind {
            if let Some(i) = self.focus.iter().position(|k| *k == key) {
                self.focus.remove(i);
            } else if self.focus.len() < 4 {
                self.focus.push(key);
            }
        } else if self.focus == [key.clone()] {
            self.focus.clear();
        } else {
            self.focus = vec![key];
        }
    }

    fn fmt(&self, kind: Kind, v: f32) -> String {
        kind.format(v, self.fahrenheit)
    }
}

pub struct Hardware {
    monitor: Option<Monitor>,
    visible: Arc<AtomicBool>,
    interval_ms: u64,
    vs: ViewState,
}

const INTERVALS: [(u64, &str); 4] = [(500, "0.5 s"), (1000, "1 s"), (2000, "2 s"), (5000, "5 s")];
const WINDOWS: [(u32, &str); 3] = [(60, "1 min"), (300, "5 min"), (600, "10 min")];

impl Hardware {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Hardware {
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        Hardware {
            monitor: None,
            visible: Arc::new(AtomicBool::new(false)),
            interval_ms: get("hw_interval").and_then(|v| v.parse().ok()).unwrap_or(1000),
            vs: ViewState {
                view: if get("hw_view").as_deref() == Some("table") { View::Table } else { View::Dashboard },
                fahrenheit: get("hw_fahrenheit").as_deref() == Some("1"),
                window: get("hw_window").and_then(|v| v.parse().ok()).unwrap_or(300),
                focus: Vec::new(),
                collapsed: HashSet::new(),
                reset: false,
                events: Vec::new(),
            },
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string("hw_interval", self.interval_ms.to_string());
        storage.set_string("hw_view", if self.vs.view == View::Table { "table" } else { "dashboard" }.into());
        storage.set_string("hw_fahrenheit", if self.vs.fahrenheit { "1" } else { "0" }.into());
        storage.set_string("hw_window", self.vs.window.to_string());
    }

    /// The page isn't on screen: keep sampling (for min/max and logging) but
    /// stop repainting the window for it.
    pub fn hidden(&self) {
        self.visible.store(false, Ordering::Relaxed);
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.vs.events)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, elevated: bool) {
        self.visible.store(true, Ordering::Relaxed);
        let monitor = self.monitor.get_or_insert_with(|| {
            let ctx = ui.ctx().clone();
            let visible = self.visible.clone();
            Monitor::start(self.interval_ms, move || {
                if visible.load(Ordering::Relaxed) {
                    ctx.request_repaint();
                }
            })
        });
        let vs = &mut self.vs;
        let interval_ms = &mut self.interval_ms;

        egui::CentralPanel::default().show(ui, |ui| {
            let snap = monitor.lock();
            header(ui, &snap, vs, monitor, interval_ms);
            driver_banner(ui, &snap, vs, elevated);
            if snap.samples == 0 {
                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading sensors…");
                });
                return;
            }
            egui::ScrollArea::vertical().id_salt("hw_scroll").auto_shrink([false, false]).show(ui, |ui| {
                if !vs.focus.is_empty() {
                    focus_card(ui, &snap, vs);
                    ui.add_space(10.0);
                }
                match vs.view {
                    View::Dashboard => dashboard::show(ui, &snap, vs),
                    View::Table => table::show(ui, &snap, vs),
                }
                notes(ui, &snap);
                ui.add_space(8.0);
            });
        });
        if std::mem::take(&mut vs.reset) {
            monitor.reset_stats();
        }
        #[cfg(debug_assertions)]
        self.debug_env(ui.ctx());
    }

    /// Debug builds only, for checking the page without driving the mouse:
    /// HEFT_DEBUG_HW_VIEW=table, HEFT_DEBUG_HW_FOCUS=<sensor label>, and
    /// HEFT_DEBUG_SCREENSHOT=<file.png> to save the window after a few
    /// seconds (HEFT_DEBUG_SCREENSHOT_DELAY) and quit.
    #[cfg(debug_assertions)]
    fn debug_env(&mut self, ctx: &egui::Context) {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let Some(monitor) = &self.monitor else { return };
        let samples = monitor.lock().samples;
        if samples == 1 {
            match env("HEFT_DEBUG_THEME").as_deref() {
                Some("dark") => ctx.set_theme(egui::Theme::Dark),
                Some("light") => ctx.set_theme(egui::Theme::Light),
                _ => {}
            }
            if env("HEFT_DEBUG_HW_VIEW").as_deref() == Some("table") {
                self.vs.view = View::Table;
            }
            if let Some(label) = env("HEFT_DEBUG_HW_FOCUS") {
                let snap = monitor.lock();
                let keys: Vec<_> = label
                    .split(',')
                    .filter_map(|l| {
                        snap.devices.iter().find_map(|d| d.sensors.iter().find(|s| s.label == l).map(|s| (d.key.clone(), s.key.clone())))
                    })
                    .collect();
                self.vs.focus = keys;
            }
        }
        let Some(path) = env("HEFT_DEBUG_SCREENSHOT") else { return };
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Ok(file) = std::fs::File::create(&path) {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                if let Ok(mut w) = enc.write_header() {
                    let _ = w.write_image_data(&bytes);
                }
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let delay: u64 = env("HEFT_DEBUG_SCREENSHOT_DELAY").and_then(|v| v.parse().ok()).unwrap_or(6);
        if samples == delay {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}

/// Title, what's in the machine, and the page controls.
fn header(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState, monitor: &Monitor, interval_ms: &mut u64) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new("Hardware").size(20.0).strong());
            let summary = system_summary(snap);
            ui.label(RichText::new(if summary.is_empty() { "Temperatures, fans, clocks and power, live".into() } else { summary }).weak());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            match monitor.log_status() {
                Some((path, rows)) => {
                    if ui
                        .button(format!("Stop logging ({rows} rows)"))
                        .on_hover_text(format!("Writing every reading to {}", path.display()))
                        .clicked()
                    {
                        monitor.stop_log();
                        vs.events.push(Event::Toast(format!("Saved {rows} readings to {}", path.display()), false));
                    }
                }
                None => {
                    if ui.button("Log to CSV…").on_hover_text("Save every reading to a spreadsheet file while this runs").clicked()
                        && let Some(p) = rfd::FileDialog::new()
                            .set_title("Log sensor readings to")
                            .set_file_name("heft-sensors.csv")
                            .add_filter("CSV", &["csv"])
                            .save_file()
                        && let Err(e) = monitor.start_log(&p)
                    {
                        vs.events.push(Event::Toast(format!("Could not start logging: {e}"), true));
                    }
                }
            }
            if ui.button("Reset min/max").on_hover_text("Start the minimum, maximum and average over from now").clicked() {
                vs.reset = true;
            }
            let unit = if vs.fahrenheit { "°F" } else { "°C" };
            if ui.button(unit).on_hover_text("Switch between Celsius and Fahrenheit").clicked() {
                vs.fahrenheit = !vs.fahrenheit;
            }
            let label = INTERVALS.iter().find(|i| i.0 == *interval_ms).map(|i| i.1).unwrap_or("1 s");
            egui::ComboBox::from_id_salt("hw_interval").selected_text(format!("Every {label}")).width(92.0).show_ui(ui, |ui| {
                for (ms, name) in INTERVALS {
                    if ui.selectable_value(interval_ms, ms, format!("Every {name}")).changed() {
                        monitor.set_interval_ms(ms);
                    }
                }
            });
            ui.separator();
            for (v, name) in [(View::Table, "All sensors"), (View::Dashboard, "Dashboard")] {
                if ui.add(egui::Button::new(name).selected(vs.view == v)).clicked() {
                    vs.view = v;
                }
            }
        });
    });
    ui.add_space(8.0);
}

/// "AMD Ryzen 7 9800X3D · NVIDIA GeForce RTX 3090 · 32 GB"
fn system_summary(snap: &Snapshot) -> String {
    let mut parts: Vec<String> = Vec::new();
    for class in [Class::Cpu, Class::Gpu] {
        let mut devs: Vec<_> = snap.devices.iter().filter(|d| d.class == class).collect();
        // The integrated GPU is rarely the interesting one when there's another.
        if class == Class::Gpu && devs.len() > 1 {
            devs.retain(|d| d.detail != "Integrated");
        }
        parts.extend(devs.iter().map(|d| short_name(&d.name)));
    }
    if let Some(m) = snap.devices.iter().find(|d| d.class == Class::Memory) {
        parts.push(m.detail.replace(" installed", " RAM"));
    }
    parts.join("  ·  ")
}

fn short_name(name: &str) -> String {
    name.replace("(R)", "").replace("(TM)", "").replace(" Processor", "").replace("  ", " ").trim().to_string()
}

/// What the driver situation means for the user, and what they can do.
fn driver_banner(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState, elevated: bool) {
    let (title, body) = match &snap.driver {
        Driver::NotNeeded | Driver::Active(_) => return,
        Driver::NotInstalled => (
            "CPU temperature, power and motherboard fans need PawnIO",
            "PawnIO is a free, signed, open-source driver (the one LibreHardwareMonitor and FanControl use). Heft never installs drivers itself. Everything else on this page works without it.",
        ),
        Driver::NeedsAdmin => (
            "Restart as administrator to read CPU temperature and motherboard sensors",
            "PawnIO is installed, but Windows only lets administrators use it.",
        ),
        Driver::Failed(_) => ("PawnIO couldn't be used", ""),
    };
    let p = draw::palette(ui);
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, p.warning);
                ui.label(RichText::new(title).strong());
            });
            if let Driver::Failed(e) = &snap.driver {
                ui.label(RichText::new(e).weak());
            } else {
                ui.label(RichText::new(body).weak());
            }
            ui.horizontal(|ui| match &snap.driver {
                Driver::NotInstalled => {
                    let cmd = "winget install namazso.PawnIO";
                    if ui.button("Copy install command").on_hover_text(cmd).clicked() {
                        ui.ctx().copy_text(cmd.into());
                        vs.events.push(Event::Toast(format!("Copied: {cmd}"), false));
                    }
                    ui.hyperlink_to("pawnio.eu", "https://pawnio.eu");
                    ui.label(RichText::new("Then restart Heft as administrator.").weak());
                }
                Driver::NeedsAdmin | Driver::Failed(_) if !elevated && crate::platform::CAN_ELEVATE => {
                    if ui.button("Restart as administrator").clicked() {
                        vs.events.push(Event::Elevate);
                    }
                }
                _ => {}
            });
        });
    ui.add_space(8.0);
}

/// The big chart for whatever was clicked.
fn focus_card(ui: &mut egui::Ui, snap: &Snapshot, vs: &mut ViewState) {
    let found: Vec<_> = vs.focus.iter().filter_map(|(d, s)| snap.find(d, s)).collect();
    if found.is_empty() {
        vs.focus.clear();
        return;
    }
    let kind = found[0].1.kind;
    let p = draw::palette(ui);
    let series: Vec<draw::Series> = found
        .iter()
        .enumerate()
        .map(|(i, (d, s))| draw::Series {
            label: if found.len() > 1 { format!("{} · {}", short_name(&d.name), s.label) } else { s.label.clone() },
            color: if found.len() == 1 { p.accent } else { p.series[i % p.series.len()] },
            sensor: s,
        })
        .collect();
    let mut close = false;
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if found.len() == 1 {
                    let (d, s) = found[0];
                    ui.label(RichText::new(format!("{} {}", s.label, kind.noun())).strong());
                    ui.label(RichText::new(short_name(&d.name)).weak());
                } else {
                    ui.label(RichText::new(format!("Comparing {}", kind.noun())).strong());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Close").clicked() {
                        close = true;
                    }
                    for (secs, name) in WINDOWS.iter().rev() {
                        if ui.add(egui::Button::new(*name).selected(vs.window == *secs).small()).clicked() {
                            vs.window = *secs;
                        }
                    }
                });
            });
            // Legend with the stats that matter, one line per series.
            for s in &series {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, 2.0, s.color);
                    if series.len() > 1 {
                        ui.label(&s.label);
                    }
                    let v = s.sensor.value.unwrap_or(f32::NAN);
                    ui.label(RichText::new(vs.fmt(kind, v)).strong());
                    ui.label(
                        RichText::new(format!(
                            "min {}   avg {}   max {}",
                            vs.fmt(kind, s.sensor.min),
                            vs.fmt(kind, s.sensor.avg()),
                            vs.fmt(kind, s.sensor.max)
                        ))
                        .weak(),
                    );
                });
            }
            ui.add_space(4.0);
            draw::history_chart(ui, 190.0, &snap.times, &series, kind, vs.window as f64, vs.fahrenheit);
            if found.len() == 1 {
                ui.label(RichText::new("Ctrl-click another reading of the same kind to compare.").weak().size(11.0));
            }
        });
    if close {
        vs.focus.clear();
    }
}

/// What Heft can't measure here, said plainly.
fn notes(ui: &mut egui::Ui, snap: &Snapshot) {
    let missing: Vec<_> = snap.notes.iter().filter(|n| n.level == NoteLevel::Missing).collect();
    let info: Vec<_> = snap.notes.iter().filter(|n| n.level == NoteLevel::Info).collect();
    if !missing.is_empty() {
        ui.add_space(12.0);
        ui.label(RichText::new("Not measured").strong());
        for n in missing {
            ui.label(RichText::new(&n.text).weak());
        }
    }
    ui.add_space(12.0);
    if let Driver::Active(uses) = &snap.driver {
        ui.label(RichText::new(format!("Read through the PawnIO driver: {}.", uses.join(", "))).weak().size(12.0));
    }
    for n in info {
        ui.label(RichText::new(&n.text).weak().size(12.0));
    }
    ui.label(
        RichText::new(format!("Each round of readings took {} ms.", snap.sample_cost.as_millis().max(1)))
            .weak()
            .size(12.0),
    );
}
