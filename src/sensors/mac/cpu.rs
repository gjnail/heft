//! The processor: its name and core layout from sysctl and the device tree,
//! load per core from the kernel's tick counters, and temperatures from the
//! SMC (or Apple silicon's HID die sensors when the SMC has none Heft knows).

use std::rc::Rc;

use super::cf::{text, IoObject};
use super::hid::{Group, Hid};
use super::smc::Smc;
use super::{avg_max, sysctl_string, sysctl_u64};
use crate::sensors::{Class, Frame, Kind, Note, Source};

#[derive(Clone)]
pub struct Topology {
    /// Label of each logical CPU, in the kernel's numbering.
    pub labels: Vec<String>,
    /// Logical CPUs in the order they're shown: performance cores first.
    pub order: Vec<usize>,
    /// What Apple calls each kind of core: "Super" or "Performance", "Efficiency".
    pub p_name: String,
    pub e_name: String,
    /// "4 super and 6 efficiency cores", "8 cores, 16 threads".
    pub detail: String,
}

/// Which logical CPUs are performance ('P') and efficiency ('E') cores,
/// from the device tree's cpu nodes.
fn cluster_types() -> Vec<(usize, char)> {
    let Some(cpus) = IoObject::at_path("IODeviceTree:/cpus") else { return Vec::new() };
    let mut out: Vec<(usize, char)> = cpus
        .children("IODeviceTree")
        .iter()
        .filter_map(|c| {
            let id = c.property("logical-cpu-id")?.obj().i64()?;
            let ty = text(c.property("cluster-type")?.obj())?;
            Some((id as usize, ty.chars().next()?))
        })
        .collect();
    out.sort();
    out
}

/// Core labels from the kind of each logical CPU (in logical order).
pub fn label_cores(kinds: &[char], p_name: &str, e_name: &str) -> (Vec<String>, Vec<usize>) {
    let mut labels = vec![String::new(); kinds.len()];
    let mut order = Vec::new();
    for (want, name) in [('P', p_name), ('E', e_name)] {
        for (n, (i, _)) in kinds.iter().enumerate().filter(|(_, k)| **k == want).enumerate() {
            labels[i] = format!("{name} core #{}", n + 1);
            order.push(i);
        }
    }
    (labels, order)
}

pub fn topology() -> Topology {
    let logical = sysctl_u64("hw.logicalcpu").unwrap_or(1).max(1) as usize;
    let physical = sysctl_u64("hw.physicalcpu").unwrap_or(logical as u64) as usize;
    let levels = sysctl_u64("hw.nperflevels").unwrap_or(1);
    let name = |l: u64| sysctl_string(&format!("hw.perflevel{l}.name"));
    let (p_name, e_name) = match (name(0), name(levels.saturating_sub(1))) {
        (Some(p), Some(e)) if levels >= 2 => (p, e),
        _ => ("Performance".to_string(), "Efficiency".to_string()),
    };
    let types = cluster_types();
    if levels >= 2 && types.len() == logical && types.iter().enumerate().all(|(i, t)| t.0 == i) {
        let kinds: Vec<char> = types.iter().map(|t| t.1).collect();
        let (labels, order) = label_cores(&kinds, &p_name, &e_name);
        let count = |k: char| kinds.iter().filter(|&&c| c == k).count();
        let detail = format!("{} {} and {} {} cores", count('P'), p_name.to_lowercase(), count('E'), e_name.to_lowercase());
        return Topology { labels, order, p_name, e_name, detail };
    }
    // Intel: without a way to tell which threads share a core, show threads.
    let (word, detail) = if physical == logical {
        ("Core", format!("{physical} cores"))
    } else {
        ("Thread", format!("{physical} cores, {logical} threads"))
    };
    Topology { labels: (1..=logical).map(|n| format!("{word} #{n}")).collect(), order: (0..logical).collect(), p_name, e_name, detail }
}

/// Share of time busy between two readings of one CPU's tick counters
/// (user, system, idle, nice), which wrap at 32 bits.
pub fn busy_share(prev: &[u32; 4], cur: &[u32; 4]) -> Option<f32> {
    let d: Vec<u64> = (0..4).map(|i| cur[i].wrapping_sub(prev[i]) as u64).collect();
    let busy = d[0] + d[1] + d[3];
    let total = busy + d[2];
    (total > 0).then(|| busy as f32 / total as f32 * 100.0)
}

fn ticks(host: libc::mach_port_t) -> Vec<[u32; 4]> {
    let mut count: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut info_count: libc::mach_msg_type_number_t = 0;
    let r = unsafe { libc::host_processor_info(host, libc::PROCESSOR_CPU_LOAD_INFO, &mut count, &mut info, &mut info_count) };
    if r != 0 || info.is_null() {
        return Vec::new();
    }
    let raw = unsafe { std::slice::from_raw_parts(info as *const u32, info_count as usize) };
    let out = raw.as_chunks::<4>().0.iter().take(count as usize).copied().collect();
    unsafe {
        libc::vm_deallocate(super::mach::task_self(), info as libc::vm_address_t, info_count as usize * std::mem::size_of::<i32>());
    }
    out
}

/// Where the processor's temperatures come from.
pub enum Temps {
    /// Apple silicon: every CPU core sensor, shown as average and hottest.
    Apple(Rc<Smc>, Vec<String>),
    /// Intel: individually named keys, as (key, label).
    Intel(Rc<Smc>, Vec<(String, String)>),
    /// HID sensors (indexes), with the word for what they measure.
    Hid(Rc<Hid>, Vec<usize>, &'static str),
    None,
}

impl Temps {
    /// The best source this Mac has.
    pub fn pick(smc: Option<&Rc<Smc>>, hid: Option<&Rc<Hid>>, apple_keys: Vec<String>) -> Temps {
        if let Some(smc) = smc {
            if !apple_keys.is_empty() {
                return Temps::Apple(smc.clone(), apple_keys);
            }
            let intel = intel_keys(smc);
            if !intel.is_empty() {
                return Temps::Intel(smc.clone(), intel);
            }
        }
        if let Some(hid) = hid {
            for (g, word) in [(Group::Cpu, "Core"), (Group::Soc, "SoC")] {
                let idx = hid.in_group(g);
                if !idx.is_empty() {
                    return Temps::Hid(hid.clone(), idx, word);
                }
            }
        }
        Temps::None
    }
}

/// Intel Macs' CPU temperature keys that exist on this one.
fn intel_keys(smc: &Smc) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // PECI reports the hottest core; TC0D/E/F are the die on different models.
    for (key, label) in [("TCXC", "Core max"), ("TC0D", "Die"), ("TC0E", "Die"), ("TC0F", "Die"), ("TC0P", "Proximity")] {
        if smc.has(key) && !out.iter().any(|(_, l)| l == label) {
            out.push((key.into(), label.into()));
        }
    }
    for n in 1..=9 {
        let key = format!("TC{n}C");
        if smc.has(&key) {
            out.push((key, format!("Core #{n}")));
        }
    }
    out
}

pub struct Cpu {
    name: String,
    topo: Topology,
    host: libc::mach_port_t,
    prev: Vec<[u32; 4]>,
    temps: Temps,
    /// Intel package power keys, as (key, label).
    power: Vec<(&'static str, &'static str)>,
    smc: Option<Rc<Smc>>,
}

impl Cpu {
    pub fn new(name: String, topo: Topology, temps: Temps, smc: Option<Rc<Smc>>) -> Cpu {
        let host = unsafe { super::mach::mach_host_self() };
        let power = match &temps {
            Temps::Intel(smc, _) => {
                [("PCPT", "Package"), ("PCPC", "Cores"), ("PCPG", "Graphics")].into_iter().filter(|(k, _)| smc.has(k)).collect()
            }
            _ => Vec::new(),
        };
        Cpu { prev: ticks(host), name, topo, host, temps, power, smc }
    }
}

impl Source for Cpu {
    fn sample(&mut self, frame: &mut Frame) {
        let d = frame.device(Class::Cpu, "cpu", &self.name);
        d.detail(self.topo.detail.clone());
        let cur = ticks(self.host);
        if cur.len() == self.prev.len() {
            let (mut busy, mut total) = (0u64, 0u64);
            for (p, c) in self.prev.iter().zip(&cur) {
                let t: u64 = (0..4).map(|i| c[i].wrapping_sub(p[i]) as u64).sum();
                busy += t - c[2].wrapping_sub(p[2]) as u64;
                total += t;
            }
            if total > 0 {
                d.add("Total", Kind::Load, busy as f32 / total as f32 * 100.0);
            }
            for &i in &self.topo.order {
                if let (Some(p), Some(c), Some(label)) = (self.prev.get(i), cur.get(i), self.topo.labels.get(i))
                    && let Some(v) = busy_share(p, c)
                {
                    d.add(label.clone(), Kind::Load, v);
                }
            }
        }
        self.prev = cur;

        let mut found = false;
        match &self.temps {
            Temps::Apple(smc, keys) => {
                let vals: Vec<f32> = keys.iter().filter_map(|k| smc.read(k)).filter(|t| *t > 5.0 && *t < 130.0).collect();
                if let Some((avg, max)) = avg_max(&vals) {
                    d.add("Core average", Kind::Temperature, avg);
                    d.add("Core max", Kind::Temperature, max);
                    found = true;
                }
            }
            Temps::Intel(smc, keys) => {
                let mut cores = Vec::new();
                for (key, label) in keys {
                    if let Some(t) = smc.read(key).filter(|t| *t > 5.0 && *t < 130.0) {
                        if label.starts_with("Core #") {
                            cores.push(t);
                        }
                        d.add(label.clone(), Kind::Temperature, t);
                        found = true;
                    }
                }
                if !keys.iter().any(|(_, l)| l == "Core max")
                    && let Some((_, max)) = avg_max(&cores)
                {
                    d.add("Core max", Kind::Temperature, max);
                }
            }
            Temps::Hid(hid, idx, word) => {
                let vals: Vec<f32> = idx.iter().filter_map(|&i| hid.value(i)).collect();
                if let Some((avg, max)) = avg_max(&vals) {
                    d.add(format!("{word} average"), Kind::Temperature, avg);
                    d.add(format!("{word} max"), Kind::Temperature, max);
                    found = true;
                }
            }
            Temps::None => {}
        }
        if let Some(smc) = &self.smc {
            for (key, label) in &self.power {
                if let Some(w) = smc.read(key).filter(|w| *w >= 0.0) {
                    d.add(*label, Kind::Power, w);
                }
            }
        }
        if !found {
            frame.note(Note::missing(format!("Heft can't read the temperature of this processor ({}) yet.", self.name)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_labels() {
        // M5: six efficiency cores numbered first, four super cores after.
        let kinds = ['E', 'E', 'E', 'E', 'E', 'E', 'P', 'P', 'P', 'P'];
        let (labels, order) = label_cores(&kinds, "Super", "Efficiency");
        assert_eq!(labels[0], "Efficiency core #1");
        assert_eq!(labels[5], "Efficiency core #6");
        assert_eq!(labels[6], "Super core #1");
        assert_eq!(labels[9], "Super core #4");
        assert_eq!(order, [6, 7, 8, 9, 0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn ticks_to_load() {
        assert_eq!(busy_share(&[100, 50, 800, 50], &[130, 60, 850, 60]), Some(50.0));
        assert_eq!(busy_share(&[1, 1, 1, 1], &[1, 1, 1, 1]), None);
        // Counters wrap at 32 bits.
        assert_eq!(busy_share(&[u32::MAX, 0, u32::MAX - 9, 0], &[9, 0, 0, 0]), Some(50.0));
    }
}
