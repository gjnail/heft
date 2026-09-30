//! Linux sensors. The kernel already exposes nearly everything: CPU load in
//! /proc/stat, clocks in cpufreq, and temperatures, fans, voltages and power
//! from every hwmon driver (k10temp, coretemp, nct6775, it87, amdgpu, nvme,
//! drivetemp...) under /sys/class/hwmon. No extra driver is needed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{Class, Frame, Kind, Note, Source};

pub fn sources() -> Vec<Box<dyn Source>> {
    vec![
        Box::new(Cpu::new()),
        Box::new(Memory),
        Box::new(Hwmon::default()),
        Box::new(Disks::default()),
        Box::new(Network::default()),
        Box::new(Batteries),
    ]
}

fn read(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_num(p: impl AsRef<Path>) -> Option<f64> {
    read(p)?.parse().ok()
}

fn entries(dir: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// The device a sysfs entry belongs to, so readings from different places
/// (a drive's hwmon and its block device) land on the same card.
fn device_key(p: &Path) -> Option<String> {
    fs::canonicalize(p.join("device")).ok().map(|d| d.to_string_lossy().into_owned())
}

// ----------------------------------------------------------------------
// Processor

struct Cpu {
    name: String,
    /// Logical CPU numbers of each physical core.
    cores: Vec<Vec<u32>>,
    last: HashMap<String, (u64, u64)>,
}

impl Cpu {
    fn new() -> Cpu {
        let name = read("/proc/cpuinfo")
            .and_then(|s| s.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).map(|n| n.trim().to_string()))
            .unwrap_or_else(|| "Processor".into());
        let mut by_core: Vec<((u32, u32), u32)> = Vec::new();
        for p in entries("/sys/devices/system/cpu") {
            let Some(n) = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_prefix("cpu")).and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let topo = p.join("topology");
            let pkg = read_num(topo.join("physical_package_id")).unwrap_or(0.0) as u32;
            let core = read_num(topo.join("core_id")).unwrap_or(n as f64) as u32;
            by_core.push(((pkg, core), n));
        }
        by_core.sort();
        let mut cores: Vec<Vec<u32>> = Vec::new();
        let mut prev = None;
        for (key, n) in by_core {
            if prev != Some(key) {
                cores.push(Vec::new());
                prev = Some(key);
            }
            cores.last_mut().unwrap().push(n);
        }
        Cpu { name, cores, last: HashMap::new() }
    }
}

/// "cpu3 10 0 5 100 ..." to ("cpu3", busy, total) jiffies.
fn parse_stat_line(line: &str) -> Option<(String, u64, u64)> {
    let mut it = line.split_whitespace();
    let name = it.next()?.to_string();
    let v: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((name, total - idle, total))
}

impl Source for Cpu {
    fn sample(&mut self, frame: &mut Frame) {
        let d = frame.device(Class::Cpu, "cpu", &self.name);
        let mut load: HashMap<String, f32> = HashMap::new();
        for line in read("/proc/stat").unwrap_or_default().lines().filter(|l| l.starts_with("cpu")) {
            let Some((name, busy, total)) = parse_stat_line(line) else { continue };
            if let Some(&(pb, pt)) = self.last.get(&name)
                && total > pt
            {
                load.insert(name.clone(), (busy.saturating_sub(pb)) as f32 / (total - pt) as f32 * 100.0);
            }
            self.last.insert(name, (busy, total));
        }
        if let Some(t) = load.get("cpu") {
            d.add("Total", Kind::Load, *t);
        }
        let mut clocks = Vec::new();
        for (i, threads) in self.cores.iter().enumerate() {
            let label = format!("Core #{}", i + 1);
            let loads: Vec<f32> = threads.iter().filter_map(|n| load.get(&format!("cpu{n}")).copied()).collect();
            if !loads.is_empty() {
                d.add(label.clone(), Kind::Load, loads.iter().sum::<f32>() / loads.len() as f32);
            }
            let khz = threads
                .iter()
                .filter_map(|n| read_num(format!("/sys/devices/system/cpu/cpu{n}/cpufreq/scaling_cur_freq")))
                .fold(f64::NAN, f64::max);
            if khz > 0.0 {
                d.add(label, Kind::Clock, (khz / 1000.0) as f32);
                clocks.push(khz / 1000.0);
            }
        }
        if !clocks.is_empty() {
            d.add("Average", Kind::Clock, (clocks.iter().sum::<f64>() / clocks.len() as f64) as f32);
        }
    }
}

// ----------------------------------------------------------------------
// Memory

struct Memory;

impl Source for Memory {
    fn sample(&mut self, frame: &mut Frame) {
        let info = read("/proc/meminfo").unwrap_or_default();
        let kb = |key: &str| {
            info.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).map(|v| v * 1024.0)
        };
        let (Some(total), Some(avail)) = (kb("MemTotal:"), kb("MemAvailable:")) else { return };
        let used = total - avail;
        let d = frame.device(Class::Memory, "memory", "Memory");
        d.detail(format!("{} installed", crate::util::fmt_size(total as u64)));
        d.add("Physical", Kind::Load, (used / total * 100.0) as f32);
        d.add("Used", Kind::Data, used as f32);
        d.add("Available", Kind::Data, avail as f32);
        if let (Some(st), Some(sf)) = (kb("SwapTotal:"), kb("SwapFree:"))
            && st > 0.0
        {
            d.add("Swap", Kind::Load, ((st - sf) / st * 100.0) as f32);
        }
    }
}

// ----------------------------------------------------------------------
// hwmon: every sensor driver the kernel has loaded

#[derive(Default)]
struct Hwmon {
    /// Fans that have spun at least once (unconnected headers read 0).
    spun: HashSet<String>,
}

/// Where a hwmon driver's readings belong.
fn classify(driver: &str) -> Option<Class> {
    Some(match driver {
        "k10temp" | "coretemp" | "zenpower" | "cpu_thermal" => Class::Cpu,
        "amdgpu" | "radeon" | "nouveau" | "i915" | "xe" => Class::Gpu,
        "nvme" | "drivetemp" => Class::Storage,
        "acpitz" => Class::Other,
        d if d.starts_with("nct6") || d.starts_with("it87") || d.starts_with("it86") => Class::Motherboard,
        "asusec" | "asus_ec_sensors" | "asus_wmi_sensors" | "dell_smm" | "thinkpad" | "f71882fg" | "w83627ehf" | "gigabyte_wmi" => {
            Class::Motherboard
        }
        _ => return None,
    })
}

/// Linux labels to the names the rest of Heft uses.
fn tidy_label(driver: &str, label: &str) -> String {
    if let Some(n) = label.strip_prefix("Tccd").and_then(|n| n.parse::<u32>().ok()) {
        return format!("CCD #{n}");
    }
    if let Some(n) = label.strip_prefix("Core ").and_then(|n| n.parse::<u32>().ok()) {
        return format!("Core #{}", n + 1);
    }
    match (driver, label) {
        ("k10temp", "Tctl") => "Tctl/Tdie".into(),
        (_, l) if l.starts_with("Package id") => "Package".into(),
        ("nvme", "Composite") => "Drive".into(),
        ("drivetemp", _) => "Drive".into(),
        (_, l) => l.to_string(),
    }
}

/// "temp10_input" sorts after "temp2_input": (kind, channel number, field).
fn natural_key(f: &str) -> (String, u32, String) {
    let (chan, field) = f.split_once('_').unwrap_or((f, ""));
    let kind: String = chan.chars().take_while(|c| !c.is_ascii_digit()).collect();
    let n = chan[kind.len()..].parse().unwrap_or(0);
    (kind, n, field.to_string())
}

fn board_name() -> String {
    let vendor = read("/sys/class/dmi/id/board_vendor").unwrap_or_default();
    let name = read("/sys/class/dmi/id/board_name").unwrap_or_default();
    match (vendor.is_empty(), name.is_empty()) {
        (_, true) => "Motherboard".into(),
        (true, false) => name,
        (false, false) => format!("{vendor} {name}"),
    }
}

impl Source for Hwmon {
    fn sample(&mut self, frame: &mut Frame) {
        let board = board_name();
        let mut saw_cpu_temp = false;
        for dir in entries("/sys/class/hwmon") {
            let Some(driver) = read(dir.join("name")) else { continue };
            let class = classify(&driver).unwrap_or(Class::Other);
            let dev_path = device_key(&dir);
            let (key, name) = match class {
                Class::Cpu => ("cpu".to_string(), String::new()),
                Class::Motherboard => ("board".to_string(), board.clone()),
                Class::Storage => (
                    format!("disk:{}", dev_path.clone().unwrap_or_else(|| driver.clone())),
                    read(dir.join("device/model")).unwrap_or_else(|| "Drive".into()),
                ),
                Class::Gpu => (format!("gpu:{}", dev_path.clone().unwrap_or_else(|| driver.clone())), gpu_name(&driver)),
                _ => (format!("hwmon:{driver}"), if driver == "acpitz" { "ACPI thermal zones".into() } else { driver.clone() }),
            };
            let d = frame.device(class, &key, &name);
            if class == Class::Motherboard {
                d.detail(format!("{driver} driver"));
            }
            let mut files: Vec<String> =
                fs::read_dir(&dir).map(|r| r.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()).unwrap_or_default();
            files.sort_by_key(|f| natural_key(f));
            for f in &files {
                let Some((chan, field)) = f.split_once('_') else { continue };
                let (kind, scale) = match (chan.trim_end_matches(|c: char| c.is_ascii_digit()), field) {
                    ("temp", "input") => (Kind::Temperature, 0.001),
                    ("fan", "input") => (Kind::Fan, 1.0),
                    ("in", "input") => (Kind::Voltage, 0.001),
                    ("curr", "input") => (Kind::Current, 0.001),
                    ("power", "average") | ("power", "input") => (Kind::Power, 1e-6),
                    _ => continue,
                };
                let Some(raw) = read_num(dir.join(f)) else { continue };
                let v = (raw * scale) as f32;
                let label = read(dir.join(format!("{chan}_label"))).map(|l| tidy_label(&driver, &l)).unwrap_or_else(|| match kind {
                    Kind::Temperature if class == Class::Storage && chan == "temp1" => "Drive".into(),
                    Kind::Fan => format!("Fan #{}", chan.trim_start_matches("fan")),
                    _ => chan.to_string(),
                });
                match kind {
                    Kind::Temperature if !(-40.0..150.0).contains(&v) || v == 0.0 => continue,
                    Kind::Temperature => {
                        saw_cpu_temp |= class == Class::Cpu;
                        // The driver's own limits, where it has them (NVMe, coretemp).
                        let limit = |field: &str| read_num(dir.join(format!("{chan}_{field}"))).map(|t| (t / 1000.0) as f32).filter(|t| *t > 20.0 && *t < 150.0);
                        d.add(label, kind, v).limits(limit("max"), limit("crit"));
                    }
                    Kind::Fan => {
                        let id = format!("{key}/{chan}");
                        if v > 0.0 {
                            self.spun.insert(id.clone());
                        }
                        if !self.spun.contains(&id) {
                            continue;
                        }
                        d.add(label.clone(), kind, v);
                        let pwm = chan.replace("fan", "pwm");
                        if let Some(p) = read_num(dir.join(&pwm)) {
                            d.add(label, Kind::Duty, (p / 2.55) as f32);
                        }
                    }
                    Kind::Power if class == Class::Gpu => {
                        d.add("Board", kind, v);
                    }
                    _ => {
                        d.add(label, kind, v);
                    }
                }
            }
            if class == Class::Gpu {
                gpu_extras(&dir.join("device"), d);
            }
        }
        if Path::new("/proc/driver/nvidia").exists() {
            frame.note(Note::info("NVIDIA cards on the proprietary driver aren't read on Linux yet; nvidia-smi shows their temperature."));
        }
        if !saw_cpu_temp {
            frame.note(Note::missing(
                "No CPU temperature driver is loaded. On most systems that's k10temp (AMD) or coretemp (Intel): try `sudo modprobe k10temp` or `sudo modprobe coretemp`.",
            ));
        }
    }
}

fn gpu_name(driver: &str) -> String {
    match driver {
        "amdgpu" | "radeon" => "AMD graphics".into(),
        "nouveau" => "NVIDIA graphics".into(),
        "i915" | "xe" => "Intel graphics".into(),
        d => d.into(),
    }
}

/// amdgpu's load, memory and clocks, which live next to its hwmon.
fn gpu_extras(dev: &Path, d: &mut super::FrameDevice) {
    if let Some(v) = read_num(dev.join("gpu_busy_percent")) {
        d.add("Core", Kind::Load, v as f32);
    }
    if let (Some(used), Some(total)) = (read_num(dev.join("mem_info_vram_used")), read_num(dev.join("mem_info_vram_total")))
        && total > 0.0
    {
        d.add("Memory used", Kind::Data, used as f32);
        d.add("Memory", Kind::Level, (used / total * 100.0) as f32);
    }
}

// ----------------------------------------------------------------------
// Disks: activity, throughput and space

#[derive(Default)]
struct Disks {
    /// Disk name to (sectors read, sectors written, ms busy) and when.
    last: HashMap<String, (u64, u64, u64, Instant)>,
}

/// A line of /proc/diskstats to (name, sectors read, sectors written, ms doing I/O).
fn parse_diskstats(line: &str) -> Option<(String, u64, u64, u64)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 13 {
        return None;
    }
    let n = |i: usize| f[i].parse::<u64>().ok();
    Some((f[2].to_string(), n(5)?, n(9)?, n(12)?))
}

/// Used and total bytes across the mounted partitions of one disk.
fn space(disk: &str) -> Option<(u64, u64)> {
    let mounts = read("/proc/mounts")?;
    let mut seen = HashSet::new();
    let (mut used, mut total) = (0u64, 0u64);
    for line in mounts.lines() {
        let mut it = line.split_whitespace();
        let (Some(dev), Some(at)) = (it.next(), it.next()) else { continue };
        let Some(part) = dev.strip_prefix("/dev/") else { continue };
        if !part.starts_with(disk) || !seen.insert(part.to_string()) {
            continue;
        }
        let at = at.replace("\\040", " ");
        let c = std::ffi::CString::new(at).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
            let frag = st.f_frsize as u64;
            total += st.f_blocks as u64 * frag;
            used += (st.f_blocks as u64 - st.f_bfree as u64) * frag;
        }
    }
    (total > 0).then_some((used, total))
}

impl Source for Disks {
    fn sample(&mut self, frame: &mut Frame) {
        let now = Instant::now();
        for line in read("/proc/diskstats").unwrap_or_default().lines() {
            let Some((name, rd, wr, busy)) = parse_diskstats(line) else { continue };
            let sys = PathBuf::from(format!("/sys/block/{name}"));
            // Whole disks only, and not loop, RAM or device-mapper devices.
            if !sys.join("device").exists() || ["loop", "ram", "zram", "dm-"].iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let key = format!("disk:{}", device_key(&sys).unwrap_or_else(|| name.clone()));
            let model = read(sys.join("device/model")).filter(|m| !m.is_empty()).unwrap_or_else(|| name.clone());
            let d = frame.device(Class::Storage, &key, &model);
            let rotational = read(sys.join("queue/rotational")).as_deref() == Some("1");
            let bus = if name.starts_with("nvme") {
                "NVMe"
            } else if fs::canonicalize(&sys).map(|p| p.to_string_lossy().contains("/usb")).unwrap_or(false) {
                "USB"
            } else {
                "SATA"
            };
            let size = read_num(sys.join("size")).map(|s| s * 512.0).unwrap_or(0.0);
            let mut detail = vec![bus.to_string(), if rotational { "Hard drive".into() } else { "SSD".into() }];
            if size > 0.0 {
                detail.push(if size >= 1e12 { format!("{:.1} TB", size / 1e12) } else { format!("{:.0} GB", size / 1e9) });
            }
            d.detail(detail.join(" · "));
            if let Some(&(prd, pwr, pbusy, at)) = self.last.get(&name) {
                let dt = now.duration_since(at).as_secs_f64();
                if dt > 0.05 {
                    d.add("Activity", Kind::Load, ((busy.saturating_sub(pbusy)) as f64 / (dt * 1000.0) * 100.0).min(100.0) as f32);
                    d.add("Read", Kind::Rate, (rd.saturating_sub(prd) as f64 * 512.0 / dt) as f32);
                    d.add("Write", Kind::Rate, (wr.saturating_sub(pwr) as f64 * 512.0 / dt) as f32);
                }
            }
            if let Some((used, total)) = space(&name) {
                d.add("Space used", Kind::Level, used as f32 / total as f32 * 100.0);
            }
            self.last.insert(name, (rd, wr, busy, now));
        }
    }
}

// ----------------------------------------------------------------------
// Network

#[derive(Default)]
struct Network {
    last: HashMap<String, (u64, u64, Instant)>,
}

impl Source for Network {
    fn sample(&mut self, frame: &mut Frame) {
        let now = Instant::now();
        for p in entries("/sys/class/net") {
            let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
            // Physical adapters have a device behind them; bridges, VPNs and lo don't.
            if !p.join("device").exists() || read(p.join("operstate")).as_deref() != Some("up") {
                continue;
            }
            let (Some(rx), Some(tx)) = (read_num(p.join("statistics/rx_bytes")), read_num(p.join("statistics/tx_bytes"))) else {
                continue;
            };
            let (rx, tx) = (rx as u64, tx as u64);
            let d = frame.device(Class::Network, &format!("net:{name}"), &name);
            if let Some(mbps) = read_num(p.join("speed")).filter(|s| *s > 0.0) {
                d.detail(if mbps >= 1000.0 { format!("{} Gbps", mbps / 1000.0) } else { format!("{mbps} Mbps") });
            }
            if let Some(&(prx, ptx, at)) = self.last.get(&name) {
                let dt = now.duration_since(at).as_secs_f64();
                if dt > 0.05 {
                    d.add("Download", Kind::Rate, (rx.saturating_sub(prx) as f64 / dt) as f32);
                    d.add("Upload", Kind::Rate, (tx.saturating_sub(ptx) as f64 / dt) as f32);
                }
            }
            self.last.insert(name, (rx, tx, now));
        }
    }
}

// ----------------------------------------------------------------------
// Batteries

struct Batteries;

impl Source for Batteries {
    fn sample(&mut self, frame: &mut Frame) {
        for p in entries("/sys/class/power_supply") {
            if read(p.join("type")).as_deref() != Some("Battery") {
                continue;
            }
            let id = p.file_name().and_then(|n| n.to_str()).unwrap_or("BAT").to_string();
            let maker = read(p.join("manufacturer")).unwrap_or_default();
            let model = read(p.join("model_name")).unwrap_or_default();
            let title = format!("{maker} {model}").trim().to_string();
            let d = frame.device(Class::Battery, &format!("battery:{id}"), if title.is_empty() { &id } else { &title });
            if let Some(c) = read_num(p.join("cycle_count")).filter(|c| *c > 0.0) {
                d.detail(format!("{c} charge cycles"));
            }
            let volts = read_num(p.join("voltage_now")).map(|v| v / 1e6);
            let design_v = read_num(p.join("voltage_min_design")).map(|v| v / 1e6).or(volts);
            // Energy in µWh, or charge in µAh times the design voltage.
            let energy = |f: &str| {
                read_num(p.join(format!("energy_{f}")))
                    .map(|v| v / 1e6)
                    .or_else(|| Some(read_num(p.join(format!("charge_{f}")))? / 1e6 * design_v?))
            };
            let (design, full, now) = (energy("full_design"), energy("full"), energy("now"));
            if let Some(v) = design {
                d.add("Designed capacity", Kind::Energy, v as f32);
            }
            if let Some(v) = full {
                d.add("Full charge capacity", Kind::Energy, v as f32);
            }
            if let Some(v) = now {
                d.add("Remaining capacity", Kind::Energy, v as f32);
            }
            if let (Some(de), Some(fu)) = (design, full)
                && de > 0.0
            {
                d.add("Wear", Kind::Level, (100.0 - fu / de * 100.0).max(0.0) as f32);
            }
            if let Some(c) = read_num(p.join("capacity")) {
                d.add("Charge", Kind::Level, c as f32);
            }
            if let Some(v) = volts {
                d.add("Battery", Kind::Voltage, v as f32);
            }
            let watts = read_num(p.join("power_now"))
                .map(|w| w / 1e6)
                .or_else(|| Some(read_num(p.join("current_now"))? / 1e6 * volts?));
            if let Some(w) = watts.filter(|w| *w > 0.0) {
                let status = read(p.join("status"));
                if status.as_deref() == Some("Charging") {
                    d.add("Charge rate", Kind::Power, w as f32);
                } else if status.as_deref() == Some("Discharging") {
                    d.add("Discharge rate", Kind::Power, w as f32);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines() {
        assert_eq!(parse_stat_line("cpu0 10 0 5 80 5 0 0 0 0 0"), Some(("cpu0".into(), 15, 100)));
        assert_eq!(parse_stat_line("cpu"), None);
    }

    #[test]
    fn diskstats() {
        let l = "   8       0 sda 1000 10 20000 500 400 20 8000 300 0 700 800";
        assert_eq!(parse_diskstats(l), Some(("sda".into(), 20000, 8000, 700)));
    }

    #[test]
    fn labels() {
        assert_eq!(tidy_label("k10temp", "Tctl"), "Tctl/Tdie");
        assert_eq!(tidy_label("k10temp", "Tccd1"), "CCD #1");
        assert_eq!(tidy_label("coretemp", "Core 0"), "Core #1");
        assert_eq!(tidy_label("coretemp", "Package id 0"), "Package");
        assert_eq!(tidy_label("nvme", "Composite"), "Drive");
    }
}
