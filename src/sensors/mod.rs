//! Hardware sensors: temperatures, loads, clocks, fans, voltages and power.
//!
//! Each platform backend is a list of [`Source`]s. A background thread asks
//! every source for its current readings once per interval and merges them
//! into a [`Snapshot`], which keeps the latest value, min, max, average and a
//! few minutes of history for every sensor.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How many samples of history each sensor keeps (10 minutes at 1 per second).
pub const HISTORY: usize = 600;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Class {
    Cpu,
    Gpu,
    Memory,
    Motherboard,
    Storage,
    Network,
    Battery,
    /// ACPI thermal zones and anything else that isn't clearly one device.
    Other,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Cpu => "Processor",
            Class::Gpu => "Graphics",
            Class::Memory => "Memory",
            Class::Motherboard => "Motherboard",
            Class::Storage => "Drive",
            Class::Network => "Network",
            Class::Battery => "Battery",
            Class::Other => "Other",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    /// Degrees Celsius.
    Temperature,
    /// Percent busy.
    Load,
    /// MHz.
    Clock,
    /// Watts.
    Power,
    /// Volts.
    Voltage,
    /// Amperes.
    Current,
    /// Revolutions per minute.
    Fan,
    /// Fan drive in percent (PWM duty).
    Duty,
    /// Bytes.
    Data,
    /// Bytes per second.
    Rate,
    /// Percent of some capacity (battery charge, wear, space used).
    Level,
    /// Watt-hours.
    Energy,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Temperature,
        Kind::Load,
        Kind::Clock,
        Kind::Power,
        Kind::Voltage,
        Kind::Current,
        Kind::Fan,
        Kind::Duty,
        Kind::Data,
        Kind::Rate,
        Kind::Level,
        Kind::Energy,
    ];

    /// Group heading, as in the sensor table.
    pub fn group(self) -> &'static str {
        match self {
            Kind::Temperature => "Temperatures",
            Kind::Load => "Utilization",
            Kind::Clock => "Clocks",
            Kind::Power => "Power",
            Kind::Voltage => "Voltages",
            Kind::Current => "Currents",
            Kind::Fan => "Fans",
            Kind::Duty => "Fan control",
            Kind::Data => "Data",
            Kind::Rate => "Throughput",
            Kind::Level => "Levels",
            Kind::Energy => "Capacities",
        }
    }

    /// What one reading of this kind is, for titles ("CPU fan speed").
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Temperature => "temperature",
            Kind::Load => "load",
            Kind::Clock => "clock",
            Kind::Power => "power",
            Kind::Voltage => "voltage",
            Kind::Current => "current",
            Kind::Fan => "fan speed",
            Kind::Duty => "fan drive",
            Kind::Data => "amount",
            Kind::Rate => "throughput",
            Kind::Level => "level",
            Kind::Energy => "capacity",
        }
    }

    pub fn unit(self, fahrenheit: bool) -> &'static str {
        match self {
            Kind::Temperature if fahrenheit => "°F",
            Kind::Temperature => "°C",
            Kind::Load | Kind::Duty | Kind::Level => "%",
            Kind::Clock => "MHz",
            Kind::Power => "W",
            Kind::Voltage => "V",
            Kind::Current => "A",
            Kind::Fan => "RPM",
            Kind::Data => "B",
            Kind::Rate => "B/s",
            Kind::Energy => "Wh",
        }
    }

    /// The value in display units (only temperatures convert).
    pub fn display(self, v: f32, fahrenheit: bool) -> f32 {
        if self == Kind::Temperature && fahrenheit { v * 9.0 / 5.0 + 32.0 } else { v }
    }

    /// Value and unit, e.g. "54.3 °C", "4,725 MHz", "12.4 MB/s".
    pub fn format(self, v: f32, fahrenheit: bool) -> String {
        if !v.is_finite() {
            return "-".into();
        }
        let d = self.display(v, fahrenheit);
        match self {
            Kind::Temperature => format!("{d:.1} {}", self.unit(fahrenheit)),
            Kind::Load | Kind::Duty | Kind::Level => format!("{d:.0} %"),
            Kind::Clock => format!("{} MHz", crate::util::fmt_count(d.max(0.0).round() as u64)),
            Kind::Power | Kind::Current => format!("{d:.1} {}", self.unit(false)),
            Kind::Voltage => format!("{d:.3} V"),
            Kind::Fan => format!("{} RPM", crate::util::fmt_count(d.max(0.0).round() as u64)),
            Kind::Data => crate::util::fmt_size(d.max(0.0) as u64),
            Kind::Rate => format!("{}/s", crate::util::fmt_size(d.max(0.0) as u64)),
            Kind::Energy => format!("{d:.1} Wh"),
        }
    }

    /// Just the number, for compact places (gauges, CSV).
    pub fn format_number(self, v: f32, fahrenheit: bool) -> String {
        if !v.is_finite() {
            return "-".into();
        }
        let d = self.display(v, fahrenheit);
        match self {
            Kind::Temperature | Kind::Power | Kind::Current | Kind::Energy => format!("{d:.1}"),
            Kind::Voltage => format!("{d:.3}"),
            _ => format!("{d:.0}"),
        }
    }
}

// ----------------------------------------------------------------------
// What sources report

/// One round of readings from every source.
#[derive(Default)]
pub struct Frame {
    devices: Vec<FrameDevice>,
    notes: Vec<Note>,
}

pub struct FrameDevice {
    key: String,
    name: String,
    class: Class,
    detail: String,
    readings: Vec<Reading>,
}

impl Frame {
    /// The device with this key, added if this is its first reading.
    pub fn device(&mut self, class: Class, key: &str, name: &str) -> &mut FrameDevice {
        match self.devices.iter().position(|d| d.key == key) {
            Some(i) => &mut self.devices[i],
            None => {
                self.devices.push(FrameDevice {
                    key: key.to_string(),
                    name: name.to_string(),
                    class,
                    detail: String::new(),
                    readings: Vec::new(),
                });
                self.devices.last_mut().unwrap()
            }
        }
    }

    /// Something the user should know about what is (or isn't) measured.
    pub fn note(&mut self, note: Note) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

impl FrameDevice {
    /// Model, interface or chip name, shown under the device name.
    pub fn detail(&mut self, detail: impl Into<String>) -> &mut Self {
        self.detail = detail.into();
        self
    }

    /// A reading. The label must be unique within the device and kind; it
    /// is also what identifies the sensor from one sample to the next.
    pub fn add(&mut self, label: impl Into<String>, kind: Kind, value: f32) -> &mut Self {
        if value.is_finite() {
            let label = label.into();
            let key = format!("{kind:?}/{label}");
            self.readings.push(Reading { key, label, kind, value, limits: None });
        }
        self
    }

    /// The hardware's own warning and critical levels for the reading just
    /// added (a drive's thermal limits, a GPU's slowdown temperature).
    pub fn limits(&mut self, warn: Option<f32>, crit: Option<f32>) -> &mut Self {
        if let Some(r) = self.readings.last_mut()
            && (warn.is_some() || crit.is_some())
        {
            r.limits = Some(Limits { warn, crit });
        }
        self
    }
}

struct Reading {
    key: String,
    label: String,
    kind: Kind,
    value: f32,
    limits: Option<Limits>,
}

/// Levels the hardware itself says are too hot (or too high).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub warn: Option<f32>,
    pub crit: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub level: NoteLevel,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteLevel {
    Info,
    Missing,
}

impl Note {
    pub fn info(text: impl Into<String>) -> Note {
        Note { level: NoteLevel::Info, text: text.into() }
    }
    pub fn missing(text: impl Into<String>) -> Note {
        Note { level: NoteLevel::Missing, text: text.into() }
    }
}

/// A platform's way of reading sensors. Sources are created and used on the
/// sampling thread only.
pub trait Source {
    fn sample(&mut self, frame: &mut Frame);
}

/// The low-level access Heft needs for CPU and motherboard sensors.
#[derive(Clone, Debug, PartialEq)]
// Only Windows needs a driver; elsewhere these states never happen.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Driver {
    /// This platform reads everything without extra help (Linux hwmon), or
    /// there's nothing more a driver could add.
    NotNeeded,
    /// PawnIO isn't installed.
    NotInstalled,
    /// PawnIO is installed but Heft isn't running as administrator.
    NeedsAdmin,
    /// Loaded; lists what it's being used for.
    Active(Vec<String>),
    Failed(String),
}

// ----------------------------------------------------------------------
// The merged state the UI reads

pub struct Sensor {
    pub key: String,
    pub label: String,
    pub kind: Kind,
    /// Latest reading, `None` if the last sample didn't include it.
    pub value: Option<f32>,
    pub min: f32,
    pub max: f32,
    sum: f64,
    count: u64,
    /// One entry per sample in [`Snapshot::times`]; NaN where missing.
    pub history: VecDeque<f32>,
    pub limits: Option<Limits>,
}

impl Sensor {
    pub fn avg(&self) -> f32 {
        if self.count == 0 { f32::NAN } else { (self.sum / self.count as f64) as f32 }
    }

    fn reset_stats(&mut self) {
        match self.value {
            Some(v) => {
                self.min = v;
                self.max = v;
                self.sum = v as f64;
                self.count = 1;
            }
            None => {
                self.min = f32::NAN;
                self.max = f32::NAN;
                self.sum = 0.0;
                self.count = 0;
            }
        }
    }
}

pub struct Device {
    pub key: String,
    pub name: String,
    pub class: Class,
    pub detail: String,
    pub sensors: Vec<Sensor>,
    last_seen: u64,
}

impl Device {
    pub fn sensor(&self, kind: Kind, label: &str) -> Option<&Sensor> {
        self.sensors.iter().find(|s| s.kind == kind && s.label == label)
    }

    /// First sensor of this kind whose label is one of `labels`, in order of preference.
    pub fn first_of(&self, kind: Kind, labels: &[&str]) -> Option<&Sensor> {
        labels.iter().find_map(|l| self.sensor(kind, l))
    }

    pub fn of_kind(&self, kind: Kind) -> impl Iterator<Item = &Sensor> {
        self.sensors.iter().filter(move |s| s.kind == kind)
    }
}

pub struct Snapshot {
    pub devices: Vec<Device>,
    /// Seconds since monitoring started, one per sample.
    pub times: VecDeque<f64>,
    pub samples: u64,
    pub notes: Vec<Note>,
    pub driver: Driver,
    /// How long the last round of reading took.
    pub sample_cost: Duration,
}

impl Snapshot {
    fn new() -> Snapshot {
        Snapshot {
            devices: Vec::new(),
            times: VecDeque::new(),
            samples: 0,
            notes: Vec::new(),
            driver: Driver::NotNeeded,
            sample_cost: Duration::ZERO,
        }
    }

    pub fn find(&self, device: &str, sensor: &str) -> Option<(&Device, &Sensor)> {
        let d = self.devices.iter().find(|d| d.key == device)?;
        let s = d.sensors.iter().find(|s| s.key == sensor)?;
        Some((d, s))
    }

    pub fn reset_stats(&mut self) {
        for d in &mut self.devices {
            for s in &mut d.sensors {
                s.reset_stats();
            }
        }
    }

    fn merge(&mut self, frame: Frame, t: f64) {
        self.samples += 1;
        let n = self.samples;
        self.times.push_back(t);
        if self.times.len() > HISTORY {
            self.times.pop_front();
        }
        let len = self.times.len();
        // Every history is now one short of `times`; this round fills the gap.
        for d in &mut self.devices {
            for s in &mut d.sensors {
                s.value = None;
                while s.history.len() >= len {
                    s.history.pop_front();
                }
            }
        }

        for fd in frame.devices {
            let idx = match self.devices.iter().position(|d| d.key == fd.key) {
                Some(i) => i,
                None => {
                    // Keep devices grouped by class, in the order they first appeared.
                    let at = self.devices.iter().position(|d| d.class > fd.class).unwrap_or(self.devices.len());
                    self.devices.insert(
                        at,
                        Device {
                            key: fd.key.clone(),
                            name: String::new(),
                            class: fd.class,
                            detail: String::new(),
                            sensors: Vec::new(),
                            last_seen: n,
                        },
                    );
                    at
                }
            };
            let dev = &mut self.devices[idx];
            dev.name = fd.name;
            dev.detail = fd.detail;
            dev.last_seen = n;
            for Reading { key, label, kind, value: v, limits } in fd.readings {
                let s = match dev.sensors.iter().position(|s| s.key == key) {
                    Some(i) => &mut dev.sensors[i],
                    None => {
                        let mut history = VecDeque::with_capacity(HISTORY + 1);
                        history.resize(len - 1, f32::NAN);
                        dev.sensors.push(Sensor {
                            key,
                            label: String::new(),
                            kind,
                            value: None,
                            min: v,
                            max: v,
                            sum: 0.0,
                            count: 0,
                            history,
                            limits: None,
                        });
                        dev.sensors.last_mut().unwrap()
                    }
                };
                s.label = label;
                if limits.is_some() {
                    s.limits = limits;
                }
                if s.value.is_some() {
                    // Two readings with the same label in one frame: keep the first.
                    continue;
                }
                s.value = Some(v);
                s.min = if s.min.is_nan() { v } else { s.min.min(v) };
                s.max = if s.max.is_nan() { v } else { s.max.max(v) };
                s.sum += v as f64;
                s.count += 1;
                s.history.push_back(v);
            }
        }

        // Everything not read this round gets a gap in its history.
        for d in &mut self.devices {
            for s in &mut d.sensors {
                if s.history.len() < len {
                    s.history.push_back(f32::NAN);
                }
            }
        }
        // Devices gone for a while (an unplugged USB drive) are dropped.
        self.devices.retain(|d| n - d.last_seen < 30);
        self.notes = frame.notes;
    }
}

// ----------------------------------------------------------------------
// The sampling thread

struct Shared {
    snap: Mutex<Snapshot>,
    interval_ms: AtomicU64,
    stop: AtomicBool,
    log: Mutex<Option<CsvLog>>,
}

/// Samples sensors on a background thread for as long as it lives.
pub struct Monitor {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    /// Start sampling. `on_sample` runs after every round (to repaint the UI).
    pub fn start(interval_ms: u64, on_sample: impl Fn() + Send + 'static) -> Monitor {
        let shared = Arc::new(Shared {
            snap: Mutex::new(Snapshot::new()),
            interval_ms: AtomicU64::new(interval_ms.max(250)),
            stop: AtomicBool::new(false),
            log: Mutex::new(None),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("heft-sensors".into())
            .spawn(move || run(&sh, on_sample))
            .ok();
        Monitor { shared, thread }
    }

    pub fn lock(&self) -> MutexGuard<'_, Snapshot> {
        self.shared.snap.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_interval_ms(&self, ms: u64) {
        self.shared.interval_ms.store(ms.max(250), Ordering::Relaxed);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }

    pub fn reset_stats(&self) {
        self.lock().reset_stats();
    }

    /// Append every sample to a CSV file from now on.
    pub fn start_log(&self, path: &Path) -> std::io::Result<()> {
        let log = CsvLog::create(path, &self.lock())?;
        *self.shared.log.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
        Ok(())
    }

    pub fn stop_log(&self) {
        *self.shared.log.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The file being logged to, and how many rows so far.
    pub fn log_status(&self) -> Option<(PathBuf, u64)> {
        self.shared.log.lock().ok()?.as_ref().map(|l| (l.path.clone(), l.rows))
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn run(sh: &Shared, on_sample: impl Fn()) {
    let (mut sources, driver) = sources();
    sh.snap.lock().unwrap_or_else(|e| e.into_inner()).driver = driver;
    let start = Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        let began = Instant::now();
        let mut frame = Frame::default();
        for s in &mut sources {
            s.sample(&mut frame);
        }
        let cost = began.elapsed();
        {
            let mut snap = sh.snap.lock().unwrap_or_else(|e| e.into_inner());
            snap.merge(frame, start.elapsed().as_secs_f64());
            snap.sample_cost = cost;
            let mut log = sh.log.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(l) = log.as_mut()
                && l.write_row(&snap).is_err()
            {
                *log = None;
            }
        }
        on_sample();
        let interval = Duration::from_millis(sh.interval_ms.load(Ordering::Relaxed));
        let due = began + interval;
        while !sh.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now >= due {
                break;
            }
            std::thread::park_timeout(due - now);
            // A new interval takes effect straight away.
            if Duration::from_millis(sh.interval_ms.load(Ordering::Relaxed)) < interval {
                break;
            }
        }
    }
}

/// Read every sensor `rounds` times, `interval` apart, on this thread. For
/// the command line.
pub fn collect(rounds: u32, interval: Duration) -> Snapshot {
    trace(|| "collect".into());
    let (mut sources, driver) = sources();
    let mut snap = Snapshot::new();
    snap.driver = driver;
    let start = Instant::now();
    for i in 0..rounds.max(1) {
        if i > 0 {
            std::thread::sleep(interval);
        }
        let began = Instant::now();
        let mut frame = Frame::default();
        for (n, s) in sources.iter_mut().enumerate() {
            let t = Instant::now();
            s.sample(&mut frame);
            trace(|| format!("round {i} source {n}: {} ms", t.elapsed().as_millis()));
        }
        snap.sample_cost = began.elapsed();
        snap.merge(frame, start.elapsed().as_secs_f64());
    }
    snap
}

/// This module is only built on Windows and Linux; macOS has no backend yet.
fn sources() -> (Vec<Box<dyn Source>>, Driver) {
    #[cfg(windows)]
    return win::sources();
    #[cfg(target_os = "linux")]
    return (linux::sources(), Driver::NotNeeded);
}

// ----------------------------------------------------------------------
// CSV logging

struct CsvLog {
    path: PathBuf,
    out: std::io::BufWriter<std::fs::File>,
    /// (device key, sensor key) for each column after the time.
    columns: Vec<(String, String)>,
    rows: u64,
}

impl CsvLog {
    fn create(path: &Path, snap: &Snapshot) -> std::io::Result<CsvLog> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut columns = Vec::new();
        let mut header = vec!["Time".to_string()];
        for d in &snap.devices {
            for s in &d.sensors {
                columns.push((d.key.clone(), s.key.clone()));
                header.push(csv_field(&format!("{} / {} [{}]", d.name, s.label, s.kind.unit(false))));
            }
        }
        writeln!(out, "{}", header.join(","))?;
        out.flush()?;
        Ok(CsvLog { path: path.to_path_buf(), out, columns, rows: 0 })
    }

    fn write_row(&mut self, snap: &Snapshot) -> std::io::Result<()> {
        let now = crate::platform::now_unix();
        // Time zones shift whole minutes, so the seconds are the same in UTC.
        let mut row = vec![format!("{}:{:02}", crate::platform::fmt_datetime(now), now.rem_euclid(60))];
        for (d, s) in &self.columns {
            row.push(match snap.find(d, s).and_then(|(_, s)| s.value) {
                Some(v) => format!("{v}"),
                None => String::new(),
            });
        }
        writeln!(self.out, "{}", row.join(","))?;
        self.out.flush()?;
        self.rows += 1;
        Ok(())
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: &[(&str, f32)]) -> Frame {
        let mut f = Frame::default();
        let d = f.device(Class::Cpu, "cpu0", "Test CPU");
        for (l, x) in v {
            d.add(*l, Kind::Temperature, *x);
        }
        f
    }

    #[test]
    fn merge_tracks_stats_and_gaps() {
        let mut s = Snapshot::new();
        s.merge(frame(&[("Package", 40.0)]), 0.0);
        s.merge(frame(&[("Package", 60.0), ("Core #1", 50.0)]), 1.0);
        s.merge(frame(&[("Core #1", 55.0)]), 2.0);
        let d = &s.devices[0];
        let pkg = d.sensor(Kind::Temperature, "Package").unwrap();
        assert_eq!(pkg.min, 40.0);
        assert_eq!(pkg.max, 60.0);
        assert_eq!(pkg.avg(), 50.0);
        assert_eq!(pkg.value, None);
        assert_eq!(pkg.history.len(), 3);
        assert!(pkg.history[2].is_nan());
        let core = d.sensor(Kind::Temperature, "Core #1").unwrap();
        assert!(core.history[0].is_nan());
        assert_eq!(core.history[2], 55.0);
        assert_eq!(core.value, Some(55.0));
    }

    #[test]
    fn history_is_capped() {
        let mut s = Snapshot::new();
        for i in 0..HISTORY + 50 {
            s.merge(frame(&[("Package", i as f32)]), i as f64);
        }
        assert_eq!(s.times.len(), HISTORY);
        let pkg = s.devices[0].sensor(Kind::Temperature, "Package").unwrap();
        assert_eq!(pkg.history.len(), HISTORY);
        assert_eq!(*pkg.history.back().unwrap(), (HISTORY + 49) as f32);
    }

    #[test]
    fn formats() {
        assert_eq!(Kind::Temperature.format(50.0, false), "50.0 °C");
        assert_eq!(Kind::Temperature.format(50.0, true), "122.0 °F");
        assert_eq!(Kind::Clock.format(4725.4, false), "4,725 MHz");
        assert_eq!(Kind::Voltage.format(1.2, false), "1.200 V");
        assert_eq!(Kind::Fan.format(1234.0, false), "1,234 RPM");
    }
}

/// Debugging aid: with HEFT_SENSOR_LOG set to a file path, setup and each
/// source's timing are appended there.
pub(crate) fn trace(msg: impl FnOnce() -> String) {
    if let Some(p) = std::env::var_os("HEFT_SENSOR_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{}", msg());
        }
    }
}
