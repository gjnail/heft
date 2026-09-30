//! The processor: load and clocks from performance counters for everyone;
//! temperatures and package power through PawnIO.

use std::time::Instant;

use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadGroupAffinity};

use super::pawnio::{BusMutex, Module, PCI_BUS};
use super::pdh::Query;
use crate::sensors::{Class, Frame, Kind, Note, Source};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

pub struct Identity {
    pub name: String,
    pub vendor: Vendor,
    pub family: u32,
    pub model: u32,
}

pub fn identity() -> Identity {
    crate::sensors::trace(|| "identity start".into());
    let name = crate::reg::Key::open(crate::reg::Hive::LocalMachine, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
        .and_then(|k| k.get_string("ProcessorNameString"))
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "Processor".into());
    let (vendor, family, model) = cpuid_signature();
    Identity { name, vendor, family, model }
}

#[cfg(target_arch = "x86_64")]
fn cpuid_signature() -> (Vendor, u32, u32) {
    use std::arch::x86_64::__cpuid;
    let v = __cpuid(0);
    let mut id = [0u8; 12];
    id[..4].copy_from_slice(&v.ebx.to_le_bytes());
    id[4..8].copy_from_slice(&v.edx.to_le_bytes());
    id[8..].copy_from_slice(&v.ecx.to_le_bytes());
    let vendor = match &id {
        b"AuthenticAMD" => Vendor::Amd,
        b"GenuineIntel" => Vendor::Intel,
        _ => Vendor::Other,
    };
    let eax = __cpuid(1).eax;
    let base_family = (eax >> 8) & 0xF;
    let family = if base_family == 0xF { base_family + ((eax >> 20) & 0xFF) } else { base_family };
    let mut model = (eax >> 4) & 0xF;
    if base_family == 0x6 || base_family == 0xF {
        model |= ((eax >> 16) & 0xF) << 4;
    }
    (vendor, family, model)
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_signature() -> (Vendor, u32, u32) {
    (Vendor::Other, 0, 0)
}

/// Physical cores, each as the (processor group, logical processor number)
/// of its threads.
fn cores() -> Vec<Vec<(u16, u32)>> {
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    if unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, base as *mut _, &mut len) } == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= len as usize {
        let rec = unsafe { &*(base.add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        if rec.Size == 0 {
            break;
        }
        if rec.Relationship == RelationProcessorCore {
            let p = unsafe { &rec.Anonymous.Processor };
            let ga = p.GroupMask[0];
            let threads: Vec<(u16, u32)> =
                (0..usize::BITS).filter(|b| ga.Mask & (1usize << b) != 0).map(|b| (ga.Group, b)).collect();
            if !threads.is_empty() {
                out.push(threads);
            }
        }
        off += rec.Size as usize;
    }
    out
}

pub struct Cpu {
    id: Identity,
    cores: Vec<Vec<(u16, u32)>>,
    pdh: Option<Query>,
    busy: Option<usize>,
    perf: Option<usize>,
    freq: Option<usize>,
    driver: Option<Low>,
}

/// Direct register access through PawnIO.
enum Low {
    Amd(Amd),
    Intel(Intel),
}

impl Cpu {
    pub fn new(id: Identity, module: Option<Module>) -> Cpu {
        let mut pdh = Query::new();
        let (mut busy, mut perf, mut freq) = (None, None, None);
        if let Some(q) = pdh.as_mut() {
            busy = q.add(r"\Processor Information(*)\% Processor Time");
            perf = q.add(r"\Processor Information(*)\% Processor Performance");
            freq = q.add(r"\Processor Information(*)\Processor Frequency");
            q.collect();
        }
        let cores = cores();
        let driver = module.map(|m| match id.vendor {
            Vendor::Amd => Low::Amd(Amd::new(m, &id)),
            _ => Low::Intel(Intel::new(m, &cores)),
        });
        Cpu { id, cores, pdh, busy, perf, freq, driver }
    }

    /// Whether PawnIO has a module for this processor.
    pub fn driver_module(id: &Identity) -> Option<&'static [u8]> {
        match id.vendor {
            Vendor::Amd if (0x17..=0x1A).contains(&id.family) => Some(super::pawnio::AMD_FAMILY_17),
            Vendor::Intel => Some(super::pawnio::INTEL_MSR),
            _ => None,
        }
    }
}

/// "0,5" is group 0, logical processor 5; "_Total" and "0,_Total" are sums.
fn parse_instance(name: &str) -> Option<(u16, u32)> {
    let (g, n) = name.split_once(',')?;
    Some((g.parse().ok()?, n.parse().ok()?))
}

impl Source for Cpu {
    fn sample(&mut self, frame: &mut Frame) {
        let dev = frame.device(Class::Cpu, "cpu", &self.id.name);
        if let Some(q) = &self.pdh
            && q.collect()
        {
            let busy = q.values(self.busy);
            let perf = q.values(self.perf);
            let freq = q.values(self.freq);
            let per_lp = |vals: &[(String, f64)], lp: (u16, u32)| {
                vals.iter().find(|(n, _)| parse_instance(n) == Some(lp)).map(|(_, v)| *v)
            };
            if let Some((_, v)) = busy.iter().find(|(n, _)| n == "_Total") {
                dev.add("Total", Kind::Load, v.clamp(0.0, 100.0) as f32);
            }
            // Effective clock = nominal frequency x "% Processor Performance",
            // which is how Task Manager shows speed.
            let nominal = freq.iter().find(|(n, _)| n == "_Total").map(|(_, v)| *v).unwrap_or(0.0);
            if nominal > 0.0
                && let Some((_, p)) = perf.iter().find(|(n, _)| n == "_Total")
            {
                dev.add("Average", Kind::Clock, (nominal * p / 100.0) as f32);
            }
            for (i, threads) in self.cores.iter().enumerate() {
                let label = format!("Core #{}", i + 1);
                let loads: Vec<f64> = threads.iter().filter_map(|&lp| per_lp(&busy, lp)).collect();
                if !loads.is_empty() {
                    let avg = loads.iter().sum::<f64>() / loads.len() as f64;
                    dev.add(label.clone(), Kind::Load, avg.clamp(0.0, 100.0) as f32);
                }
                let clock = threads
                    .iter()
                    .filter_map(|&lp| Some(per_lp(&freq, lp)? * per_lp(&perf, lp)? / 100.0))
                    .fold(f64::NAN, f64::max);
                if clock > 0.0 {
                    dev.add(label, Kind::Clock, clock as f32);
                }
            }
        }
        match &mut self.driver {
            Some(Low::Amd(a)) => a.sample(frame, &self.id.name),
            Some(Low::Intel(i)) => i.sample(frame, &self.id.name),
            None => {}
        }
        if self.driver.is_none() {
            frame.note(Note::missing("CPU temperature and power need the PawnIO driver."));
        }
    }
}

// ----------------------------------------------------------------------
// AMD Zen (family 17h to 1Ah)

const SMN_THM_TCON_CUR_TMP: u64 = 0x0005_9800;
const SMN_CCD_TEMP_ZEN2: u64 = 0x0005_9954;
const SMN_CCD_TEMP_ZEN4: u64 = 0x0005_9B08;
const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY_STAT: u64 = 0xC001_029B;

struct Amd {
    m: Module,
    pci: Option<BusMutex>,
    /// Tctl is offset from the real die temperature on a few early parts.
    tdie_offset: f32,
    ccd_base: Option<u64>,
    energy_unit: f64,
    last_energy: Option<(u32, Instant)>,
}

impl Amd {
    fn new(m: Module, id: &Identity) -> Amd {
        // From the Linux k10temp driver's offset table.
        let n = &id.name;
        let tdie_offset = if ["1600X", "1700X", "1800X"].iter().any(|s| n.contains(s)) {
            20.0
        } else if n.contains("Threadripper 19") || n.contains("Threadripper 29") {
            27.0
        } else if n.contains("2700X") {
            10.0
        } else {
            0.0
        };
        // Per-CCD sensors on the desktop and Threadripper parts where their
        // location is known.
        let ccd_base = match (id.family, id.model) {
            (0x17, 0x31) | (0x17, 0x71) | (0x19, 0x21) => Some(SMN_CCD_TEMP_ZEN2),
            (0x19, 0x61) | (0x1A, 0x44) => Some(SMN_CCD_TEMP_ZEN4),
            _ => None,
        };
        // Energy status unit: 1 / 2^ESU joules.
        let energy_unit = m.get("ioctl_read_msr", &[MSR_PWR_UNIT]).map(|v| 0.5f64.powi(((v >> 8) & 0x1F) as i32)).unwrap_or(0.0);
        Amd { m, pci: BusMutex::new(PCI_BUS), tdie_offset, ccd_base, energy_unit, last_energy: None }
    }

    fn smn(&self, addr: u64) -> Option<u32> {
        self.m.get("ioctl_read_smn", &[addr]).map(|v| v as u32)
    }

    fn sample(&mut self, frame: &mut Frame, name: &str) {
        let dev = frame.device(Class::Cpu, "cpu", name);
        let read = || {
            let tctl = self.smn(SMN_THM_TCON_CUR_TMP);
            let ccds: Vec<Option<u32>> = match self.ccd_base {
                Some(base) => (0..8).map(|i| self.smn(base + i * 4)).collect(),
                None => Vec::new(),
            };
            (tctl, ccds)
        };
        let got = match &self.pci {
            Some(mx) => mx.with(20, read),
            None => Some(read()),
        };
        if let Some((tctl, ccds)) = got {
            if let Some(raw) = tctl {
                let mut t = ((raw >> 21) * 125) as f32 / 1000.0;
                // RANGE_SEL, or both TJ_SEL bits on newer parts, mean the
                // reading is shifted up by 49 degrees.
                if raw & 0x8_0000 != 0 || raw & 0x3_0000 == 0x3_0000 {
                    t -= 49.0;
                }
                if self.tdie_offset > 0.0 {
                    dev.add("Tctl", Kind::Temperature, t);
                    dev.add("Tdie", Kind::Temperature, t - self.tdie_offset);
                } else {
                    dev.add("Tctl/Tdie", Kind::Temperature, t);
                }
            }
            let temps: Vec<(usize, f32)> = ccds
                .iter()
                .enumerate()
                .filter_map(|(i, raw)| {
                    let raw = (*raw)? & 0xFFF;
                    let t = raw as f32 * 0.125 - 305.0;
                    (raw > 0 && t > 0.0 && t < 125.0).then_some((i, t))
                })
                .collect();
            for &(i, t) in &temps {
                dev.add(format!("CCD #{}", i + 1), Kind::Temperature, t);
            }
        }

        if self.energy_unit > 0.0
            && let Some(e) = self.m.get("ioctl_read_msr", &[MSR_PKG_ENERGY_STAT])
        {
            let now = Instant::now();
            let e = e as u32;
            if let Some((prev, at)) = self.last_energy {
                let dt = now.duration_since(at).as_secs_f64();
                let joules = e.wrapping_sub(prev) as f64 * self.energy_unit;
                if dt > 0.05 {
                    dev.add("Package", Kind::Power, (joules / dt) as f32);
                }
            }
            self.last_energy = Some((e, now));
        }
    }
}

// ----------------------------------------------------------------------
// Intel Core

const MSR_IA32_THERM_STATUS: u64 = 0x19C;
const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;
const MSR_IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const MSR_RAPL_POWER_UNIT: u64 = 0x606;
const MSR_PKG_ENERGY_STATUS: u64 = 0x611;
const MSR_DRAM_ENERGY_STATUS: u64 = 0x619;
const MSR_PP0_ENERGY_STATUS: u64 = 0x639;
const MSR_PP1_ENERGY_STATUS: u64 = 0x641;

struct Intel {
    m: Module,
    /// First thread of each core, where that core's thermal MSR is read.
    first_threads: Vec<(u16, u32)>,
    tj_max: f32,
    energy_unit: f64,
    /// (label, MSR, last raw value and time)
    rapl: Vec<(&'static str, u64, Option<(u32, Instant)>)>,
}

impl Intel {
    fn new(m: Module, cores: &[Vec<(u16, u32)>]) -> Intel {
        let tj_max = m
            .get("ioctl_read_msr", &[MSR_TEMPERATURE_TARGET])
            .map(|v| ((v >> 16) & 0xFF) as f32)
            .filter(|&t| t > 50.0)
            .unwrap_or(100.0);
        let energy_unit =
            m.get("ioctl_read_msr", &[MSR_RAPL_POWER_UNIT]).map(|v| 0.5f64.powi(((v >> 8) & 0x1F) as i32)).unwrap_or(0.0);
        let rapl = [
            ("Package", MSR_PKG_ENERGY_STATUS),
            ("Cores", MSR_PP0_ENERGY_STATUS),
            ("Graphics", MSR_PP1_ENERGY_STATUS),
            ("Memory", MSR_DRAM_ENERGY_STATUS),
        ]
        .into_iter()
        .filter(|&(_, msr)| m.get("ioctl_read_msr", &[msr]).is_some())
        .map(|(l, msr)| (l, msr, None))
        .collect();
        Intel { first_threads: cores.iter().filter_map(|c| c.first().copied()).collect(), m, tj_max, energy_unit, rapl }
    }

    fn sample(&mut self, frame: &mut Frame, name: &str) {
        let dev = frame.device(Class::Cpu, "cpu", name);
        if let Some(v) = self.m.get("ioctl_read_msr", &[MSR_IA32_PACKAGE_THERM_STATUS]) {
            dev.add("Package", Kind::Temperature, self.tj_max - ((v >> 16) & 0x7F) as f32);
        }
        // Each core's thermal status is only readable from that core, so the
        // sampling thread visits them one by one.
        let mut hottest = f32::NAN;
        for (i, &(group, lp)) in self.first_threads.iter().enumerate() {
            let Some(v) = on_processor(group, lp, || self.m.get("ioctl_read_msr", &[MSR_IA32_THERM_STATUS])).flatten()
            else {
                continue;
            };
            if v & (1 << 31) == 0 {
                continue; // reading not valid
            }
            let t = self.tj_max - ((v >> 16) & 0x7F) as f32;
            hottest = hottest.max(t);
            dev.add(format!("Core #{}", i + 1), Kind::Temperature, t);
        }
        if hottest.is_finite() {
            dev.add("Core max", Kind::Temperature, hottest);
        }
        if self.energy_unit > 0.0 {
            let now = Instant::now();
            for (label, msr, last) in &mut self.rapl {
                let Some(e) = self.m.get("ioctl_read_msr", &[*msr]) else { continue };
                let e = e as u32;
                if let Some((prev, at)) = *last {
                    let dt = now.duration_since(at).as_secs_f64();
                    if dt > 0.05 {
                        dev.add(*label, Kind::Power, (e.wrapping_sub(prev) as f64 * self.energy_unit / dt) as f32);
                    }
                }
                *last = Some((e, now));
            }
        }
    }
}

/// Run `f` with this thread pinned to one logical processor.
fn on_processor<R>(group: u16, lp: u32, f: impl FnOnce() -> R) -> Option<R> {
    use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
    let want = GROUP_AFFINITY { Mask: 1usize << lp, Group: group, Reserved: [0; 3] };
    let mut prev: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
    let thread = unsafe { GetCurrentThread() };
    if unsafe { SetThreadGroupAffinity(thread, &want, &mut prev) } == 0 {
        return None;
    }
    let r = f();
    unsafe { SetThreadGroupAffinity(thread, &prev, std::ptr::null_mut()) };
    Some(r)
}
