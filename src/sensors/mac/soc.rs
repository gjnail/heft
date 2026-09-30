//! Apple silicon's own activity counters (IOReport): how long each CPU core,
//! CPU cluster and the GPU spent at each frequency step, and how much energy
//! the CPU, GPU, Neural Engine and memory used. Clocks come from those
//! residencies weighted by the frequency tables in the power manager's
//! device tree node, the way asitop and macmon do it. IOReport is private API
//! (/usr/lib/libIOReport.dylib), so it's looked up at run time.

use std::collections::HashMap;
use std::time::Instant;

use super::cf::{Cf, CFTypeRef, IoObject, Library, Obj};
use super::cpu::Topology;
use crate::sensors::{Class, Frame, Kind, Source};

type CopyChannels = unsafe extern "C" fn(CFTypeRef, CFTypeRef, u64, u64, u64) -> CFTypeRef;
type MergeChannels = unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef);
type CreateSubscription = unsafe extern "C" fn(CFTypeRef, CFTypeRef, *mut CFTypeRef, u64, CFTypeRef) -> CFTypeRef;
type CreateSamples = unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef) -> CFTypeRef;
type CreateDelta = unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef) -> CFTypeRef;
type GetString = unsafe extern "C" fn(CFTypeRef) -> CFTypeRef;
type IntegerValue = unsafe extern "C" fn(CFTypeRef, i32) -> i64;
type StateCount = unsafe extern "C" fn(CFTypeRef) -> i32;
type StateName = unsafe extern "C" fn(CFTypeRef, i32) -> CFTypeRef;
type StateResidency = unsafe extern "C" fn(CFTypeRef, i32) -> i64;

struct Api {
    create_samples: CreateSamples,
    create_delta: CreateDelta,
    group: GetString,
    subgroup: GetString,
    name: GetString,
    unit: GetString,
    integer: IntegerValue,
    state_count: StateCount,
    state_name: StateName,
    residency: StateResidency,
}

/// A subscription to some IOReport channel groups.
struct IoReport {
    api: Api,
    sub: Cf,
    subbed: Cf,
    prev: Option<(Cf, Instant)>,
}

struct Channel<'a> {
    group: String,
    subgroup: String,
    name: String,
    obj: Obj<'a>,
}

impl IoReport {
    fn open(groups: &[(&str, Option<&str>)]) -> Option<IoReport> {
        let lib = Library::open("/usr/lib/libIOReport.dylib")?;
        // The Get functions return strings the channel owns (checked: same
        // pointer every call, immortal retain count), so nothing to release.
        let (copy, merge, subscribe) = unsafe {
            (
                lib.get::<CopyChannels>("IOReportCopyChannelsInGroup")?,
                lib.get::<MergeChannels>("IOReportMergeChannels")?,
                lib.get::<CreateSubscription>("IOReportCreateSubscription")?,
            )
        };
        let api = unsafe {
            Api {
                create_samples: lib.get("IOReportCreateSamples")?,
                create_delta: lib.get("IOReportCreateSamplesDelta")?,
                group: lib.get("IOReportChannelGetGroup")?,
                subgroup: lib.get("IOReportChannelGetSubGroup")?,
                name: lib.get("IOReportChannelGetChannelName")?,
                unit: lib.get("IOReportChannelGetUnitLabel")?,
                integer: lib.get("IOReportSimpleGetIntegerValue")?,
                state_count: lib.get("IOReportStateGetCount")?,
                state_name: lib.get("IOReportStateGetNameForIndex")?,
                residency: lib.get("IOReportStateGetResidency")?,
            }
        };
        let mut all: Option<Cf> = None;
        for (group, subgroup) in groups {
            let g = Cf::string(group);
            let s = subgroup.map(Cf::string);
            let s_ptr = s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
            let Some(ch) = (unsafe { Cf::owned(copy(g.as_ptr(), s_ptr, 0, 0, 0)) }) else { continue };
            match &all {
                Some(a) => unsafe { merge(a.as_ptr(), ch.as_ptr(), std::ptr::null()) },
                None => all = Some(ch),
            }
        }
        let all = all?;
        let mut subbed: CFTypeRef = std::ptr::null();
        let sub = unsafe { Cf::owned(subscribe(std::ptr::null(), all.as_ptr(), &mut subbed, 0, std::ptr::null())) }?;
        let subbed = unsafe { Cf::owned(subbed) }?;
        Some(IoReport { api, sub, subbed, prev: None })
    }

    /// What changed since the last call, and over how many seconds.
    fn delta(&mut self) -> Option<(Cf, f64)> {
        let now = Instant::now();
        let cur = unsafe { Cf::owned((self.api.create_samples)(self.sub.as_ptr(), self.subbed.as_ptr(), std::ptr::null())) }?;
        let prev = self.prev.replace((cur, now));
        let (prev, at) = prev?;
        let cur = &self.prev.as_ref()?.0;
        let d = unsafe { Cf::owned((self.api.create_delta)(prev.as_ptr(), cur.as_ptr(), std::ptr::null())) }?;
        let secs = now.duration_since(at).as_secs_f64();
        (secs > 0.05).then_some((d, secs))
    }

    fn channels<'a>(&self, samples: &'a Cf) -> Vec<Channel<'a>> {
        let s = |f: GetString, o: Obj| unsafe { Obj::borrowed(f(o.as_ptr())) }.and_then(|v| v.string()).unwrap_or_default();
        let Some(list) = samples.obj().get("IOReportChannels") else { return Vec::new() };
        list.items()
            .into_iter()
            .map(|o| Channel { group: s(self.api.group, o), subgroup: s(self.api.subgroup, o), name: s(self.api.name, o), obj: o })
            .collect()
    }

    fn integer(&self, ch: &Channel) -> i64 {
        unsafe { (self.api.integer)(ch.obj.as_ptr(), 0) }
    }

    fn unit(&self, ch: &Channel) -> String {
        unsafe { Obj::borrowed((self.api.unit)(ch.obj.as_ptr())) }.and_then(|v| v.string()).unwrap_or_default()
    }

    fn states(&self, ch: &Channel) -> Vec<(String, i64)> {
        let p = ch.obj.as_ptr();
        let n = unsafe { (self.api.state_count)(p) }.clamp(0, 64);
        (0..n)
            .map(|i| {
                let name = unsafe { Obj::borrowed((self.api.state_name)(p, i)) }.and_then(|v| v.string()).unwrap_or_default();
                (name, unsafe { (self.api.residency)(p, i) })
            })
            .collect()
    }
}

/// Joules per unit of an energy counter ("mJ", "uJ", "nJ").
fn joules_per_unit(unit: &str) -> Option<f64> {
    match unit.trim() {
        "J" => Some(1.0),
        "mJ" => Some(1e-3),
        "uJ" | "µJ" => Some(1e-6),
        "nJ" => Some(1e-9),
        _ => None,
    }
}

/// Frequencies in MHz from a power manager "voltage-states" table: pairs of
/// little-endian u32 (frequency, voltage). Older chips give Hz, M4 and later
/// kHz; zero entries (the GPU's "off") are dropped.
pub fn dvfs_mhz(table: &[u8]) -> Vec<f32> {
    table
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
        .filter(|&f| f > 0.0)
        .map(|f| if f >= 1e7 { f / 1e6 } else { f / 1e3 } as f32)
        .collect()
}

/// From a channel's state residencies (idle states first, then one per
/// frequency step): the average frequency while running, if it ran, and
/// the share of time it was running. `None` if the states don't line up
/// with the frequency table.
pub fn residency(states: &[(String, i64)], mhz: &[f32]) -> Option<(Option<f32>, f32)> {
    let first = states.iter().position(|(n, _)| !matches!(n.as_str(), "IDLE" | "OFF" | "DOWN"))?;
    let steps = &states[first..];
    if mhz.is_empty() || steps.len().abs_diff(mhz.len()) > 1 {
        return None;
    }
    let total: i64 = states.iter().map(|s| s.1.max(0)).sum();
    if total <= 0 {
        return None;
    }
    let busy: i64 = steps.iter().map(|s| s.1.max(0)).sum();
    let (mut weighted, mut counted) = (0f64, 0i64);
    for ((_, r), f) in steps.iter().zip(mhz) {
        let r = (*r).max(0);
        weighted += r as f64 * *f as f64;
        counted += r;
    }
    let freq = (counted > 0).then(|| (weighted / counted as f64) as f32);
    Some((freq, busy as f32 / total as f32))
}

/// "ECPU" -> ('E', 0), "PCPU1" -> ('P', 1); anything else (ECPM, PCPU_IDLE) -> None.
pub fn cluster_of(name: &str) -> Option<(char, usize)> {
    let (kind, rest) = match name.strip_prefix("ECPU") {
        Some(r) => ('E', r),
        None => ('P', name.strip_prefix("PCPU")?),
    };
    if !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((kind, rest.parse().unwrap_or(0)))
}

/// The power manager's frequency tables: E cores, P cores, GPU.
fn frequency_tables() -> Option<(Vec<f32>, Vec<f32>, Vec<f32>)> {
    let has_tables = |o: &IoObject| o.property("voltage-states5-sram").is_some();
    let pmgr = IoObject::at_path("IODeviceTree:/arm-io/pmgr")
        .filter(has_tables)
        .or_else(|| IoObject::matching("AppleARMIODevice").into_iter().find(has_tables))?;
    let table = |key: &str| pmgr.property(key).and_then(|p| p.obj().bytes().map(dvfs_mhz)).unwrap_or_default();
    Some((table("voltage-states1-sram"), table("voltage-states5-sram"), table("voltage-states9")))
}

pub struct Soc {
    ior: IoReport,
    cpu_name: String,
    gpu_name: String,
    e_mhz: Vec<f32>,
    p_mhz: Vec<f32>,
    gpu_mhz: Vec<f32>,
    /// "Super", "Performance", "Efficiency".
    p_name: String,
    e_name: String,
    /// Joules per unit of each energy channel, looked up once.
    units: HashMap<String, Option<f64>>,
}

impl Soc {
    pub fn new(cpu_name: &str, gpu_name: &str, topo: &Topology) -> Option<Soc> {
        let ior = IoReport::open(&[
            ("Energy Model", None),
            ("CPU Stats", Some("CPU Complex Performance States")),
            ("CPU Stats", Some("CPU Core Performance States")),
            ("GPU Stats", Some("GPU Performance States")),
        ])?;
        let (e_mhz, p_mhz, gpu_mhz) = frequency_tables().unwrap_or_default();
        Some(Soc {
            ior,
            cpu_name: cpu_name.to_string(),
            gpu_name: gpu_name.to_string(),
            e_mhz,
            p_mhz,
            gpu_mhz,
            p_name: topo.p_name.clone(),
            e_name: topo.e_name.clone(),
            units: HashMap::new(),
        })
    }

    fn cluster_label(&self, kind: char, n: usize) -> String {
        let name = if kind == 'E' { &self.e_name } else { &self.p_name };
        if n == 0 { format!("{name} cores") } else { format!("{name} cores {}", n + 1) }
    }

    fn table(&self, kind: char) -> &[f32] {
        if kind == 'E' { &self.e_mhz } else { &self.p_mhz }
    }
}

impl Source for Soc {
    fn sample(&mut self, frame: &mut Frame) {
        let Some((delta, secs)) = self.ior.delta() else { return };
        let channels = self.ior.channels(&delta);

        let mut watts: Vec<(&'static str, f32)> = Vec::new();
        let mut cluster_watts: Vec<(char, f32)> = Vec::new();
        let mut gpu_watts = None;
        let mut clusters: Vec<(char, usize, f32)> = Vec::new();
        let mut cores: Vec<(char, Option<f32>)> = Vec::new();
        let (mut weighted, mut busy) = (0f64, 0f64);
        let mut gpu_clock = None;

        for ch in &channels {
            match (ch.group.as_str(), ch.subgroup.as_str()) {
                ("Energy Model", _) => {
                    let n = ch.name.as_str();
                    // "CPU Energy" (or "DIE_1_CPU Energy" on Ultra chips), "ANE"
                    // or "ANE0", "DRAM" or "DRAM0"; per-core channels are skipped.
                    let label = if n.ends_with("CPU Energy") {
                        "Cores"
                    } else if n.starts_with("ANE") {
                        "Neural Engine"
                    } else if n.starts_with("DRAM") {
                        "Memory"
                    } else if n.ends_with("GPU Energy") {
                        "GPU"
                    } else if n == "ECPU" || n == "PCPU" {
                        "cluster"
                    } else {
                        continue;
                    };
                    let scale = *self.units.entry(ch.name.clone()).or_insert_with(|| joules_per_unit(&self.ior.unit(ch)));
                    let Some(scale) = scale else { continue };
                    let w = (self.ior.integer(ch).max(0) as f64 * scale / secs) as f32;
                    match label {
                        "GPU" => *gpu_watts.get_or_insert(0.0) += w,
                        "cluster" => cluster_watts.push((if n == "ECPU" { 'E' } else { 'P' }, w)),
                        _ => match watts.iter_mut().find(|(l, _)| *l == label) {
                            Some((_, v)) => *v += w,
                            None => watts.push((label, w)),
                        },
                    }
                }
                ("CPU Stats", "CPU Complex Performance States") => {
                    let Some((kind, i)) = cluster_of(&ch.name) else { continue };
                    let table = self.table(kind);
                    // An idle cluster waits at its lowest step.
                    if let Some((f, _)) = residency(&self.ior.states(ch), table)
                        && let Some(f) = f.or(table.first().copied())
                    {
                        clusters.push((kind, i, f));
                    }
                }
                ("CPU Stats", "CPU Core Performance States") => {
                    let Some((kind, _)) = cluster_of(&ch.name) else { continue };
                    let table = self.table(kind);
                    let Some((f, share)) = residency(&self.ior.states(ch), table) else { continue };
                    if let Some(f) = f {
                        weighted += f as f64 * share as f64;
                        busy += share as f64;
                    }
                    cores.push((kind, f.or(table.first().copied())));
                }
                ("GPU Stats", "GPU Performance States") if ch.name == "GPUPH" => {
                    if let Some((f, _)) = residency(&self.ior.states(ch), &self.gpu_mhz) {
                        gpu_clock = f.or(self.gpu_mhz.first().copied());
                    }
                }
                _ => {}
            }
        }

        let d = frame.device(Class::Cpu, "cpu", &self.cpu_name);
        if busy > 0.0 {
            d.add("Average", Kind::Clock, (weighted / busy) as f32);
        }
        // Performance cores first, as in the load readings.
        for kind in ['P', 'E'] {
            for &(_, i, f) in clusters.iter().filter(|c| c.0 == kind) {
                d.add(self.cluster_label(kind, i), Kind::Clock, f);
            }
            let name = if kind == 'E' { &self.e_name } else { &self.p_name };
            for (i, (_, f)) in cores.iter().filter(|c| c.0 == kind).enumerate() {
                if let Some(f) = f {
                    d.add(format!("{name} core #{}", i + 1), Kind::Clock, *f);
                }
            }
        }
        // All cores, then each kind of core, then the rest of the chip.
        if let Some(&(label, w)) = watts.iter().find(|(l, _)| *l == "Cores") {
            d.add(label, Kind::Power, w);
        }
        // "ECPU" and "PCPU" energy are a whole cluster only on chips with one
        // cluster of that kind; with two, they'd be half the story.
        for kind in ['P', 'E'] {
            if clusters.iter().filter(|c| c.0 == kind).count() == 1
                && let Some(&(_, w)) = cluster_watts.iter().find(|c| c.0 == kind)
            {
                d.add(self.cluster_label(kind, 0), Kind::Power, w);
            }
        }
        for (label, w) in watts.into_iter().filter(|(l, _)| *l != "Cores") {
            d.add(label, Kind::Power, w);
        }
        if !self.gpu_name.is_empty() && (gpu_clock.is_some() || gpu_watts.is_some()) {
            let g = frame.device(Class::Gpu, "gpu", &self.gpu_name);
            if let Some(f) = gpu_clock {
                g.add("Core", Kind::Clock, f);
            }
            if let Some(w) = gpu_watts {
                g.add("GPU", Kind::Power, w);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(v: &[(&str, i64)]) -> Vec<(String, i64)> {
        v.iter().map(|(n, r)| (n.to_string(), *r)).collect()
    }

    #[test]
    fn frequency_tables() {
        // M5 efficiency cluster (kHz): 972 MHz, then 1,152 MHz.
        let t = [0xe0, 0xd4, 0x0e, 0x00, 0x16, 0x03, 0, 0, 0x00, 0x94, 0x11, 0x00, 0x16, 0x03, 0, 0];
        assert_eq!(dvfs_mhz(&t), vec![972.0, 1152.0]);
        // GPU (Hz), with the "off" entry first: 0, then 338 MHz.
        let g = [0, 0, 0, 0, 0x02, 0x03, 0, 0, 0x80, 0x78, 0x25, 0x14, 0x02, 0x03, 0, 0];
        assert_eq!(dvfs_mhz(&g), vec![338.0]);
    }

    #[test]
    fn residencies() {
        let mhz = [1000.0, 2000.0, 3000.0];
        // Half idle; running half the rest at 1 GHz and half at 3 GHz.
        let s = st(&[("IDLE", 50), ("V0P2", 25), ("V1P1", 0), ("V2P0", 25)]);
        assert_eq!(residency(&s, &mhz), Some((Some(2000.0), 0.5)));
        // Idle the whole time: no clock, zero load.
        let s = st(&[("IDLE", 100), ("V0P2", 0), ("V1P1", 0), ("V2P0", 0)]);
        assert_eq!(residency(&s, &mhz), Some((None, 0.0)));
        // The GPU has an extra step beyond its table; it still lines up.
        let s = st(&[("OFF", 10), ("P1", 10), ("P2", 0), ("P3", 0), ("P4", 0)]);
        assert_eq!(residency(&s, &mhz).map(|r| r.0), Some(Some(1000.0)));
        // A table that doesn't match the states is not guessed at.
        let s = st(&[("IDLE", 10), ("V0", 10)]);
        assert_eq!(residency(&s, &mhz), None);
        assert_eq!(residency(&st(&[("IDLE", 0), ("V0", 0), ("V1", 0), ("V2", 0)]), &mhz), None);
    }

    #[test]
    fn clusters() {
        assert_eq!(cluster_of("ECPU"), Some(('E', 0)));
        assert_eq!(cluster_of("PCPU1"), Some(('P', 1)));
        assert_eq!(cluster_of("PCPU3"), Some(('P', 3)));
        assert_eq!(cluster_of("ECPM"), None);
        assert_eq!(cluster_of("PCPU_IDLE"), None);
        assert_eq!(cluster_of("GPUPH"), None);
    }

    #[test]
    fn energy_units() {
        assert_eq!(joules_per_unit("mJ"), Some(1e-3));
        assert_eq!(joules_per_unit("nJ"), Some(1e-9));
        assert_eq!(joules_per_unit("24Mticks"), None);
    }
}
