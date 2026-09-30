//! Windows sensors. Everything works without extra software except CPU
//! temperature and power and the motherboard's sensor chip, which need the
//! PawnIO driver (see pawnio.rs).

mod battery;
mod cpu;
mod gpu;
mod net;
mod nvml;
mod pawnio;
mod pdh;
mod storage;
mod superio;

use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use super::{Class, Driver, Frame, Kind, Note, Source};

/// Time a setup step for HEFT_SENSOR_LOG.
fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let t = std::time::Instant::now();
    let r = f();
    super::trace(|| format!("setup {what}: {} ms", t.elapsed().as_millis()));
    r
}

pub fn sources() -> (Vec<Box<dyn Source>>, Driver) {
    let id = timed("identity", cpu::identity);
    let board = timed("board", board_name);
    let mut uses = Vec::new();
    let mut notes = Vec::new();
    let mut problem = None;
    let mut unavailable = |e: pawnio::Unavailable| {
        problem.get_or_insert(e);
    };

    let cpu_module = match cpu::Cpu::driver_module(&id) {
        Some(blob) => match timed("pawnio cpu", || pawnio::Module::load(blob)) {
            Ok(m) => {
                uses.push("CPU temperature and power".to_string());
                Some(m)
            }
            Err(e) => {
                unavailable(e);
                None
            }
        },
        None => {
            notes.push(Note::missing(format!("Heft can't read the temperature of this processor ({}) yet.", id.name)));
            None
        }
    };

    let mut sources: Vec<Box<dyn Source>> = vec![Box::new(timed("cpu", || cpu::Cpu::new(id, cpu_module)))];

    if cfg!(target_arch = "x86_64") {
        match timed("pawnio lpc", || pawnio::Module::load(pawnio::LPC_IO)) {
            Ok(m) => match superio::detect(m, &board) {
                Ok(chip) => {
                    uses.push(format!("Motherboard sensors ({})", chip.chip_name()));
                    sources.push(Box::new(chip));
                }
                Err(found) => notes.push(Note::missing(format!(
                    "No motherboard fan or voltage readings: Heft found {found}. It reads the Nuvoton chips on most ASUS and ASRock boards."
                ))),
            },
            Err(e) => unavailable(e),
        }
    }

    sources.push(Box::new(Memory));
    sources.push(Box::new(timed("gpu", gpu::Gpus::new)));
    sources.push(Box::new(timed("drives", storage::Drives::new)));
    sources.push(Box::new(net::Network::new()));
    sources.push(Box::new(timed("batteries", battery::Batteries::new)));
    if let Some(t) = timed("thermal zones", ThermalZones::new) {
        sources.push(Box::new(t));
    }
    if !notes.is_empty() {
        sources.push(Box::new(Notes(notes)));
    }

    let driver = match (uses.is_empty(), problem) {
        (false, _) => Driver::Active(uses),
        (true, Some(pawnio::Unavailable::NotInstalled)) => Driver::NotInstalled,
        (true, Some(pawnio::Unavailable::NeedsAdmin)) => Driver::NeedsAdmin,
        (true, Some(pawnio::Unavailable::Failed(e))) => Driver::Failed(e),
        (true, None) => Driver::NotNeeded,
    };
    (sources, driver)
}

/// "ASUSTeK COMPUTER INC. ROG STRIX X870E-E GAMING WIFI7 R2"
fn board_name() -> String {
    let key = crate::reg::Key::open(crate::reg::Hive::LocalMachine, r"HARDWARE\DESCRIPTION\System\BIOS");
    let get = |v: &str| key.as_ref().and_then(|k| k.get_string(v)).map(|s| s.trim().to_string()).unwrap_or_default();
    let (maker, product) = (get("BaseBoardManufacturer"), get("BaseBoardProduct"));
    match (maker.is_empty(), product.is_empty()) {
        (_, true) => "Motherboard".into(),
        (true, false) => product,
        (false, false) => format!("{maker} {product}"),
    }
}

pub(super) fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// Notes found while setting up, repeated each round so the page shows them.
struct Notes(Vec<Note>);

impl Source for Notes {
    fn sample(&mut self, frame: &mut Frame) {
        for n in &self.0 {
            frame.note(n.clone());
        }
    }
}

struct Memory;

impl Source for Memory {
    fn sample(&mut self, frame: &mut Frame) {
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 || m.ullTotalPhys == 0 {
            return;
        }
        let used = m.ullTotalPhys - m.ullAvailPhys;
        let d = frame.device(Class::Memory, "memory", "Memory");
        d.detail(format!("{} installed", crate::util::fmt_size(m.ullTotalPhys)));
        d.add("Physical", Kind::Load, used as f32 / m.ullTotalPhys as f32 * 100.0);
        d.add("Used", Kind::Data, used as f32);
        d.add("Available", Kind::Data, m.ullAvailPhys as f32);
        let commit = m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile);
        if m.ullTotalPageFile > 0 {
            d.add("Committed", Kind::Load, commit as f32 / m.ullTotalPageFile as f32 * 100.0);
            d.add("Committed", Kind::Data, commit as f32);
        }
    }
}

/// Firmware (ACPI) thermal zones. Laptops often have meaningful ones; on
/// desktops they're usually missing or a fixed number.
struct ThermalZones {
    q: pdh::Query,
    temp: Option<usize>,
}

impl ThermalZones {
    fn new() -> Option<ThermalZones> {
        let mut q = pdh::Query::new()?;
        let temp = q.add(r"\Thermal Zone Information(*)\High Precision Temperature");
        q.collect();
        (!q.values(temp).is_empty()).then_some(ThermalZones { q, temp })
    }
}

impl Source for ThermalZones {
    fn sample(&mut self, frame: &mut Frame) {
        if !self.q.collect() {
            return;
        }
        let zones = self.q.values(self.temp);
        if zones.is_empty() {
            return;
        }
        let d = frame.device(Class::Other, "acpi", "ACPI thermal zones");
        d.detail("Reported by the firmware, not a specific chip");
        for (name, tenths_kelvin) in zones {
            let c = (tenths_kelvin / 10.0 - 273.15) as f32;
            if c > -30.0 && c < 150.0 {
                let label = name.rsplit(['.', '\\']).next().unwrap_or(&name).to_string();
                d.add(label, Kind::Temperature, c);
            }
        }
    }
}
