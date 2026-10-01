//! macOS sensors, all read without administrator rights or a driver:
//! temperatures, fans and system power from the SMC, Apple silicon's
//! clocks and power from IOReport and its board sensors from the HID event
//! system (both private APIs every Mac monitor uses, looked up at run time),
//! and everything else from sysctl, Mach and the I/O Registry.

mod battery;
mod board;
mod cf;
mod cpu;
mod gpu;
mod hid;
mod net;
mod smc;
mod soc;
mod storage;

use std::ffi::CString;
use std::rc::Rc;

use super::{Class, Frame, Kind, Note, Source};

/// The few Mach calls libc only offers in deprecated form.
mod mach {
    unsafe extern "C" {
        pub fn mach_host_self() -> libc::mach_port_t;
        pub fn host_page_size(host: libc::mach_port_t, size: *mut libc::vm_size_t) -> libc::kern_return_t;
        static mach_task_self_: libc::mach_port_t;
    }

    pub fn task_self() -> libc::mach_port_t {
        unsafe { mach_task_self_ }
    }
}

/// Time a setup step for HEFT_SENSOR_LOG.
fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let t = std::time::Instant::now();
    let r = f();
    super::trace(|| format!("setup {what}: {} ms", t.elapsed().as_millis()));
    r
}

pub fn sources() -> Vec<Box<dyn Source>> {
    // Asked at run time so an Intel build running under Rosetta still reads
    // the SMC the Apple silicon way.
    let apple = sysctl_u64("hw.optional.arm64") == Some(1);
    let name = sysctl_string("machdep.cpu.brand_string")
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "Processor".into());
    let topo = timed("topology", cpu::topology);
    let smc = timed("smc", || smc::Smc::open(apple)).map(Rc::new);
    let hid = if apple { timed("hid", hid::Hid::open).map(Rc::new) } else { None };
    let (cpu_keys, gpu_keys) = match &smc {
        Some(s) if apple => timed("smc keys", || {
            let mut keys = Vec::new();
            for p in ["Tp", "Te", "Tg", "Tf"] {
                keys.extend(s.float_keys(p));
            }
            apple_temp_keys(&keys, &name)
        }),
        _ => (Vec::new(), Vec::new()),
    };
    let accel = timed("gpu", gpu::accelerators);
    let apple_gpu = accel.iter().find(|a| a.apple).map(|a| a.name.clone());

    let mut notes = Vec::new();
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    let temps = cpu::Temps::pick(smc.as_ref(), hid.as_ref(), cpu_keys);
    sources.push(Box::new(cpu::Cpu::new(name.clone(), topo.clone(), temps, smc.clone())));
    if apple {
        match timed("ioreport", || soc::Soc::new(&name, apple_gpu.as_deref().unwrap_or_default(), &topo)) {
            Some(s) => sources.push(Box::new(s)),
            None => notes.push(Note::missing("Clock speeds and power of the processor and graphics aren't available on this version of macOS.")),
        }
    } else {
        notes.push(Note::missing("Clock speeds of Intel processors can't be read on macOS without a kernel extension."));
    }
    let gpu_temps = match (&smc, &hid) {
        (Some(s), _) if !gpu_keys.is_empty() => gpu::Temps::Smc(s.clone(), gpu_keys),
        (_, Some(h)) if !h.in_group(hid::Group::Gpu).is_empty() => gpu::Temps::Hid(h.clone(), h.in_group(hid::Group::Gpu)),
        _ => gpu::Temps::None,
    };
    if !accel.is_empty() {
        sources.push(Box::new(gpu::Gpus::new(accel, gpu_temps)));
    }
    sources.push(Box::new(Memory::new()));
    sources.push(Box::new(timed("drives", || storage::Drives::new(hid.as_ref()))));
    sources.push(Box::new(timed("network", net::Network::new)));
    if let Some(b) = timed("battery", || battery::Battery::new(smc.as_ref(), hid.as_ref())) {
        sources.push(Box::new(b));
    }
    match &smc {
        Some(s) => sources.push(Box::new(timed("board", || board::Board::new(s.clone(), hid.as_ref())))),
        None => notes.push(Note::missing("No temperatures, fan speeds or system power: the System Management Controller couldn't be read.")),
    }
    if !notes.is_empty() {
        sources.push(Box::new(Notes(notes)));
    }
    sources
}

/// What `heft --sensors --report` adds to the readings: the Mac and chip,
/// and every raw temperature, fan, power, voltage and current key in the
/// SMC and every HID temperature sensor, so readings can be matched up on
/// Macs Heft hasn't been tested on. Nothing that identifies the Mac or its
/// owner (no serial numbers, names or addresses).
pub fn report_system(out: &mut String) {
    use std::fmt::Write;
    let (product, model) = board::model_names();
    let line = |out: &mut String, k: &str, v: String| {
        let _ = writeln!(out, "{k:<10}{v}");
    };
    line(out, "model", if model.is_empty() { product } else { format!("{product} ({model})") });
    line(out, "chip", sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "?".into()));
    let arm = sysctl_u64("hw.optional.arm64") == Some(1);
    let translated = sysctl_u64("sysctl.proc_translated") == Some(1);
    line(out, "arch", format!("{}{}", if arm { "Apple silicon" } else { "Intel" }, if translated { ", Heft running under Rosetta" } else { "" }));
    let levels: Vec<String> = (0..4)
        .filter_map(|l| {
            let n = sysctl_u64(&format!("hw.perflevel{l}.physicalcpu"))?;
            let name = sysctl_string(&format!("hw.perflevel{l}.name")).unwrap_or_else(|| format!("level {l}"));
            Some(format!("{n} {name}"))
        })
        .collect();
    let cores = sysctl_u64("hw.physicalcpu").map_or("?".into(), |n| n.to_string());
    line(out, "cores", if levels.is_empty() { cores } else { format!("{cores} ({})", levels.join(", ")) });
    line(out, "memory", sysctl_u64("hw.memsize").map_or("?".into(), crate::util::fmt_size));
    let os = crate::mac::output("/usr/bin/sw_vers", &["-productVersion"]).unwrap_or_default();
    let build = crate::mac::output("/usr/bin/sw_vers", &["-buildVersion"]).unwrap_or_default();
    line(out, "macOS", format!("{} ({})", os.trim(), build.trim()));
}

/// The raw part of the report, after the readings.
pub fn report_raw(out: &mut String) {
    use std::fmt::Write;
    let apple = sysctl_u64("hw.optional.arm64") == Some(1);
    let _ = writeln!(out, "SMC keys (temperature, fans, power, voltage, current)");
    match smc::Smc::open(apple) {
        Some(s) => {
            let keys = s.keys();
            let _ = writeln!(out, "  {} keys in all", keys.len());
            for k in keys.iter().filter(|k| k.starts_with(['T', 'F', 'P', 'V', 'I'])) {
                let Some((kind, bytes)) = s.raw(k) else { continue };
                let kind = String::from_utf8_lossy(&kind).into_owned();
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let value = smc::decode(kind.as_bytes().try_into().unwrap_or(b"????"), &bytes, apple)
                    .map_or("-".to_string(), |v| format!("{v:.2}"));
                let _ = writeln!(out, "  {k:<5} {kind:<5} {hex:<18} {value:>12}");
            }
        }
        None => {
            let _ = writeln!(out, "  couldn't be opened");
        }
    }
    let _ = writeln!(out, "\nHID temperature sensors");
    match hid::Hid::open() {
        Some(h) => {
            let mut v: Vec<(usize, &(String, hid::Group))> = h.sensors.iter().enumerate().collect();
            v.sort_by_key(|(_, (n, _))| hid::natural_key(n));
            for (i, (name, group)) in v {
                let value = h.value(i).map_or("-".to_string(), |t| format!("{t:.1}"));
                let name = if name.is_empty() { "(no name)" } else { name.as_str() };
                let _ = writeln!(out, "  {name:<34} {:<8} {value:>8}", format!("{group:?}"));
            }
        }
        None => {
            let _ = writeln!(out, "  none (Intel Macs have none)");
        }
    }
}

/// Which of Apple silicon's SMC float keys are CPU core and GPU sensors:
/// "Tp" and "Te" for the cores and "Tg" for the GPU on every generation but
/// the M3, which uses "Tf0x"/"Tf4x" for performance cores and "Tf1x"/"Tf2x"
/// for the GPU.
pub fn apple_temp_keys(keys: &[String], chip: &str) -> (Vec<String>, Vec<String>) {
    let m3 = chip.split_whitespace().any(|w| w == "M3");
    let (mut cpu, mut gpu) = (Vec::new(), Vec::new());
    for k in keys {
        let b = k.as_bytes();
        match (b.get(1), b.get(2)) {
            (Some(b'p' | b'e'), _) => cpu.push(k.clone()),
            (Some(b'g'), _) => gpu.push(k.clone()),
            (Some(b'f'), Some(b'0' | b'4')) if m3 => cpu.push(k.clone()),
            (Some(b'f'), Some(b'1' | b'2')) if m3 => gpu.push(k.clone()),
            _ => {}
        }
    }
    (cpu, gpu)
}

/// Mean and highest of some temperatures.
pub fn avg_max(v: &[f32]) -> Option<(f32, f32)> {
    if v.is_empty() {
        return None;
    }
    Some((v.iter().sum::<f32>() / v.len() as f32, v.iter().copied().fold(f32::MIN, f32::max)))
}

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let c = CString::new(name).ok()?;
    let mut len = 0usize;
    if unsafe { libc::sysctlbyname(c.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) } != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    if unsafe { libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut _, &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

pub fn sysctl_string(name: &str) -> Option<String> {
    let b = sysctl_raw(name)?;
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    Some(String::from_utf8_lossy(&b[..end]).trim().to_string()).filter(|s| !s.is_empty())
}

pub fn sysctl_u64(name: &str) -> Option<u64> {
    let b = sysctl_raw(name)?;
    match b.len() {
        4 => Some(u32::from_ne_bytes(b[..4].try_into().ok()?) as u64),
        8 => Some(u64::from_ne_bytes(b[..8].try_into().ok()?)),
        _ => None,
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

// ----------------------------------------------------------------------
// Memory

/// Page counts from the kernel, in the terms Activity Monitor uses.
#[derive(Default)]
pub struct Pages {
    pub internal: u64,
    pub purgeable: u64,
    pub external: u64,
    pub wired: u64,
    pub compressed: u64,
}

/// (app memory, wired, compressed, cached files) in bytes. "Memory used" in
/// Activity Monitor is the first three together.
pub fn memory_split(p: &Pages, page: u64) -> (u64, u64, u64, u64) {
    let app = p.internal.saturating_sub(p.purgeable) * page;
    (app, p.wired * page, p.compressed * page, (p.external + p.purgeable) * page)
}

struct Memory {
    host: libc::mach_port_t,
    total: u64,
    page: u64,
}

impl Memory {
    fn new() -> Memory {
        let mut page: libc::vm_size_t = 0;
        let host = unsafe { mach::mach_host_self() };
        if unsafe { mach::host_page_size(host, &mut page) } != 0 || page == 0 {
            page = 4096;
        }
        Memory { host, total: sysctl_u64("hw.memsize").unwrap_or(0), page: page as u64 }
    }
}

impl Source for Memory {
    fn sample(&mut self, frame: &mut Frame) {
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let r = unsafe { libc::host_statistics64(self.host, libc::HOST_VM_INFO64, &mut vm as *mut _ as *mut i32, &mut count) };
        if r != 0 || self.total == 0 {
            return;
        }
        let pages = Pages {
            internal: vm.internal_page_count as u64,
            purgeable: vm.purgeable_count as u64,
            external: vm.external_page_count as u64,
            wired: vm.wire_count as u64,
            compressed: vm.compressor_page_count as u64,
        };
        let (app, wired, compressed, cached) = memory_split(&pages, self.page);
        let used = (app + wired + compressed).min(self.total);
        let d = frame.device(Class::Memory, "memory", "Memory");
        d.detail(format!("{} installed", crate::util::fmt_size(self.total)));
        d.add("Physical", Kind::Load, used as f32 / self.total as f32 * 100.0);
        d.add("Used", Kind::Data, used as f32);
        d.add("Available", Kind::Data, (self.total - used) as f32);
        d.add("App memory", Kind::Data, app as f32);
        d.add("Wired", Kind::Data, wired as f32);
        d.add("Compressed", Kind::Data, compressed as f32);
        d.add("Cached files", Kind::Data, cached as f32);
        if let Some(b) = sysctl_raw("vm.swapusage")
            && b.len() >= std::mem::size_of::<libc::xsw_usage>()
        {
            let sw: libc::xsw_usage = unsafe { std::ptr::read_unaligned(b.as_ptr() as *const libc::xsw_usage) };
            d.add("Swap used", Kind::Data, sw.xsu_used as f32);
            if sw.xsu_total > 0 {
                d.add("Swap", Kind::Load, sw.xsu_used as f32 / sw.xsu_total as f32 * 100.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_keys() {
        let keys: Vec<String> = ["Te04", "Tg0C", "Tp00", "Tf04", "Tf14", "TfC0"].iter().map(|s| s.to_string()).collect();
        let (cpu, gpu) = apple_temp_keys(&keys, "Apple M5");
        assert_eq!(cpu, ["Te04", "Tp00"]);
        assert_eq!(gpu, ["Tg0C"]);
        let (cpu, gpu) = apple_temp_keys(&keys, "Apple M3 Pro");
        assert_eq!(cpu, ["Te04", "Tp00", "Tf04"]);
        assert_eq!(gpu, ["Tg0C", "Tf14"]);
    }

    #[test]
    fn memory_terms() {
        let p = Pages { internal: 100, purgeable: 10, external: 50, wired: 20, compressed: 5 };
        assert_eq!(memory_split(&p, 16384), (90 * 16384, 20 * 16384, 5 * 16384, 60 * 16384));
    }

    #[test]
    fn averages() {
        assert_eq!(avg_max(&[40.0, 60.0]), Some((50.0, 60.0)));
        assert_eq!(avg_max(&[]), None);
    }

    #[test]
    fn reads_sysctl() {
        assert!(sysctl_u64("hw.memsize").unwrap_or(0) > 0);
        assert!(sysctl_string("hw.model").is_some());
    }
}
