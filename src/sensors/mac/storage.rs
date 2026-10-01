//! Drives: throughput from each disk driver's statistics, temperature and
//! wear from NVMe SMART (through the IOKit plug-in, no administrator
//! needed), and space used across the volumes on the drive.

use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::cf::{Cf, IoObject, UuidBytes, IOCreatePlugInInterfaceForService};
use super::hid::{Group, Hid};
use crate::sensors::{Class, Frame, Kind, Source};

/// How often drives are asked for their temperature (as on Windows).
const TEMP_EVERY: Duration = Duration::from_secs(3);
/// How often to look for drives that were plugged in or removed.
const RESCAN_EVERY: Duration = Duration::from_secs(15);

// ----------------------------------------------------------------------
// NVMe SMART through NVMeSMARTLib.plugin (IOKit/storage/nvme/NVMeSMARTLibExternal.h)

#[repr(C)]
struct IUnknownVtbl {
    _reserved: *mut c_void,
    query_interface: unsafe extern "C" fn(*mut c_void, UuidBytes, *mut *mut c_void) -> i32,
    _add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
    release: unsafe extern "C" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct SmartVtbl {
    base: IUnknownVtbl,
    _version: u16,
    _revision: u16,
    read_data: unsafe extern "C" fn(*mut c_void, *mut u8) -> i32,
    identify: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i32,
}

const SMART_CLIENT: [u8; 16] = [0xAA, 0x0F, 0xA6, 0xF9, 0xC2, 0xD6, 0x45, 0x7F, 0xB1, 0x0B, 0x59, 0xA1, 0x32, 0x53, 0x29, 0x2F];
const SMART_INTERFACE: [u8; 16] = [0xCC, 0xD1, 0xDB, 0x19, 0xFD, 0x9A, 0x4D, 0xAF, 0xBF, 0x95, 0x12, 0x45, 0x4B, 0x23, 0x0A, 0xB6];
const PLUGIN_INTERFACE: [u8; 16] = [0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F];

/// An open NVMe SMART interface to one drive.
struct Smart {
    plugin: *mut *mut IUnknownVtbl,
    iface: *mut *mut SmartVtbl,
}

impl Smart {
    fn open(device: &IoObject) -> Option<Smart> {
        let mut plugin: *mut *mut c_void = std::ptr::null_mut();
        let mut score = 0i32;
        let (client, kind) = (Cf::uuid(UuidBytes(SMART_CLIENT)), Cf::uuid(UuidBytes(PLUGIN_INTERFACE)));
        let r = unsafe { IOCreatePlugInInterfaceForService(device.raw(), client.as_ptr(), kind.as_ptr(), &mut plugin, &mut score) };
        if r != 0 || plugin.is_null() {
            return None;
        }
        let plugin = plugin as *mut *mut IUnknownVtbl;
        let mut iface: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { ((**plugin).query_interface)(plugin as *mut c_void, UuidBytes(SMART_INTERFACE), &mut iface) };
        if hr != 0 || iface.is_null() {
            unsafe { ((**plugin).release)(plugin as *mut c_void) };
            return None;
        }
        Some(Smart { plugin, iface: iface as *mut *mut SmartVtbl })
    }

    fn log(&self) -> Option<[u8; 512]> {
        let mut buf = [0u8; 512];
        (unsafe { ((**self.iface).read_data)(self.iface as *mut c_void, buf.as_mut_ptr()) } == 0).then_some(buf)
    }

    /// The controller's warning and critical temperatures, if it states them.
    fn limits(&self) -> (Option<f32>, Option<f32>) {
        let mut buf = vec![0u8; 4096];
        if unsafe { ((**self.iface).identify)(self.iface as *mut c_void, buf.as_mut_ptr(), 0) } != 0 {
            return (None, None);
        }
        identify_limits(&buf)
    }
}

impl Drop for Smart {
    fn drop(&mut self) {
        unsafe {
            ((**self.iface).base.release)(self.iface as *mut c_void);
            ((**self.plugin).release)(self.plugin as *mut c_void);
        }
    }
}

fn kelvin(k: u16) -> Option<f32> {
    let c = k as f32 - 273.15;
    (k > 0 && c > -20.0 && c < 150.0).then_some(c)
}

/// Temperature (°C) and share of rated life used, from the SMART / Health
/// log page (NVMe 1.0 section 5.10.1.2).
pub fn parse_smart(log: &[u8]) -> (Option<f32>, Option<f32>) {
    let temp = log.get(1..3).and_then(|b| kelvin(u16::from_le_bytes([b[0], b[1]])));
    let used = log.get(5).map(|&u| u as f32);
    (temp, used)
}

/// WCTEMP and CCTEMP from Identify Controller data (Kelvin, 0 when unset).
pub fn identify_limits(id: &[u8]) -> (Option<f32>, Option<f32>) {
    let at = |o: usize| id.get(o..o + 2).and_then(|b| kelvin(u16::from_le_bytes([b[0], b[1]])));
    (at(266), at(268))
}

// ----------------------------------------------------------------------

struct Drive {
    /// Registry ID of the disk driver, which identifies the drive.
    id: u64,
    driver: IoObject,
    name: String,
    detail: String,
    internal: bool,
    smart: Option<Smart>,
    limits: (Option<f32>, Option<f32>),
    /// Last temperature and life used, and when they were read.
    health: (Option<f32>, Option<f32>),
    health_at: Option<Instant>,
    last: Option<(u64, u64, Instant)>,
}

/// Drive makers count in powers of 1000, so a "2 TB" drive says 2 TB here too.
fn fmt_decimal_size(bytes: u64) -> String {
    let tb = bytes as f64 / 1e12;
    if tb >= 1.0 { format!("{tb:.1} TB") } else { format!("{:.0} GB", bytes as f64 / 1e9) }
}

/// "Apple Fabric" and "PCI-Express" to what people call them.
fn bus_name(interconnect: &str, internal: bool) -> String {
    match interconnect {
        "Apple Fabric" | "PCI-Express" | "PCI" if internal => "Internal".into(),
        "PCI-Express" | "PCI" => "PCIe".into(),
        "Secure Digital" => "SD card".into(),
        other => other.into(),
    }
}

fn scan() -> Vec<Drive> {
    let mut out = Vec::new();
    for driver in IoObject::matching("IOBlockStorageDriver") {
        let Some(device) = driver.parent() else { continue };
        let proto = device.property("Protocol Characteristics");
        let proto = proto.as_ref().map(|p| p.obj());
        let get = |o: Option<super::cf::Obj>, k: &str| o.and_then(|o| o.get(k)).and_then(|v| v.string()).unwrap_or_default();
        let interconnect = get(proto, "Physical Interconnect");
        // Disk images and other virtual disks aren't drives.
        if interconnect == "Virtual Interface" {
            continue;
        }
        // Only drives with media in them (not an empty card reader).
        let Some(media) = driver.children("IOService").into_iter().find(|c| c.conforms_to("IOMedia")) else { continue };
        let size = media.property("Size").and_then(|s| s.obj().i64()).unwrap_or(0).max(0) as u64;
        let chars = device.property("Device Characteristics");
        let chars = chars.as_ref().map(|c| c.obj());
        let vendor = get(chars, "Vendor Name").trim().to_string();
        let product = get(chars, "Product Name").trim().to_string();
        let name = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
        let internal = get(proto, "Physical Interconnect Location") == "Internal";
        let mut detail = Vec::new();
        if !interconnect.is_empty() {
            detail.push(bus_name(&interconnect, internal));
        }
        match get(chars, "Medium Type").as_str() {
            "Solid State" => detail.push("SSD".into()),
            "Rotational" => detail.push("Hard drive".into()),
            _ => {}
        }
        if size > 0 {
            detail.push(fmt_decimal_size(size));
        }
        let smart = device.property("NVMe SMART Capable").and_then(|v| v.obj().bool()).unwrap_or(false).then(|| Smart::open(&device)).flatten();
        let limits = smart.as_ref().map(|s| s.limits()).unwrap_or((None, None));
        out.push(Drive {
            id: driver.id(),
            driver,
            name: if name.is_empty() { "Drive".into() } else { name },
            detail: detail.join(" · "),
            internal,
            smart,
            limits,
            health: (None, None),
            health_at: None,
            last: None,
        });
    }
    out
}

/// "/dev/disk3s1s1" on APFS to "disk3" (every volume in a container shares
/// its space), otherwise the partition itself.
pub fn space_group(from: &str, fstype: &str) -> Option<String> {
    let bsd = from.strip_prefix("/dev/")?;
    if !bsd.starts_with("disk") {
        return None;
    }
    if fstype != "apfs" {
        return Some(bsd.to_string());
    }
    let digits = bsd[4..].chars().take_while(|c| c.is_ascii_digit()).count();
    (digits > 0).then(|| bsd[..4 + digits].to_string())
}

/// The disk driver (by registry ID) a BSD disk sits on, following APFS
/// containers and partitions down to the physical drive.
fn drive_of(bsd: &str) -> Option<u64> {
    let mut o = IoObject::bsd(bsd)?;
    for _ in 0..16 {
        if o.conforms_to("IOBlockStorageDriver") {
            return Some(o.id());
        }
        o = o.parent()?;
    }
    None
}

fn c_str(buf: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

/// Mount points to read space from, grouped by drive: one per APFS
/// container or other file system.
fn mounts() -> HashMap<u64, Vec<CString>> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        return HashMap::new();
    }
    let all = unsafe { std::slice::from_raw_parts(list, n as usize) };
    let mut seen = Vec::new();
    let mut out: HashMap<u64, Vec<CString>> = HashMap::new();
    for m in all {
        let Some(group) = space_group(&c_str(&m.f_mntfromname), &c_str(&m.f_fstypename)) else { continue };
        if seen.contains(&group) {
            continue;
        }
        seen.push(group.clone());
        let Some(id) = drive_of(&group) else { continue };
        if let Ok(p) = CString::new(c_str(&m.f_mntonname)) {
            out.entry(id).or_default().push(p);
        }
    }
    out
}

fn space(points: &[CString]) -> Option<(u64, u64)> {
    let (mut used, mut total) = (0u64, 0u64);
    for p in points {
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(p.as_ptr(), &mut st) } == 0 {
            let b = st.f_bsize as u64;
            total += st.f_blocks * b;
            used += st.f_blocks.saturating_sub(st.f_bfree) * b;
        }
    }
    (total > 0).then_some((used, total))
}

pub struct Drives {
    drives: Vec<Drive>,
    mounts: HashMap<u64, Vec<CString>>,
    scanned: Instant,
    /// Apple silicon's NAND sensor, for the internal drive if SMART is silent.
    nand: Option<(Rc<Hid>, Vec<usize>)>,
}

impl Drives {
    pub fn new(hid: Option<&Rc<Hid>>) -> Drives {
        let nand = hid.map(|h| (h.clone(), h.in_group(Group::Drive))).filter(|(_, i)| !i.is_empty());
        Drives { drives: scan(), mounts: mounts(), scanned: Instant::now(), nand }
    }
}

impl Source for Drives {
    fn sample(&mut self, frame: &mut Frame) {
        if self.scanned.elapsed() >= RESCAN_EVERY {
            // Keep what's known about drives that are still here.
            let mut old = std::mem::take(&mut self.drives);
            self.drives = scan()
                .into_iter()
                .map(|d| match old.iter().position(|o| o.id == d.id) {
                    Some(i) => old.swap_remove(i),
                    None => d,
                })
                .collect();
            self.mounts = mounts();
            self.scanned = Instant::now();
        }
        let now = Instant::now();
        for drive in &mut self.drives {
            let d = frame.device(Class::Storage, &format!("disk:{}", drive.id), &drive.name);
            d.detail(drive.detail.clone());
            if let Some(smart) = &drive.smart
                && drive.health_at.is_none_or(|t| t.elapsed() >= TEMP_EVERY)
            {
                drive.health = smart.log().map(|l| parse_smart(&l)).unwrap_or((None, None));
                drive.health_at = Some(now);
            }
            let (warn, crit) = drive.limits;
            let mut temp = drive.health.0;
            if temp.is_none()
                && drive.internal
                && let Some((hid, idx)) = &self.nand
            {
                temp = idx.iter().filter_map(|&i| hid.value(i)).reduce(f32::max);
            }
            if let Some(t) = temp {
                d.add("Drive", Kind::Temperature, t).limits(warn, crit);
            }
            if let Some(used) = drive.health.1 {
                d.add("Life used", Kind::Level, used);
            }
            let stats = drive.driver.property("Statistics");
            let num = |k: &str| stats.as_ref().and_then(|s| s.obj().get(k)).and_then(|v| v.i64()).map(|v| v.max(0) as u64);
            if let (Some(rd), Some(wr)) = (num("Bytes (Read)"), num("Bytes (Write)")) {
                if let Some((prd, pwr, at)) = drive.last {
                    let dt = now.duration_since(at).as_secs_f64();
                    if dt > 0.05 && rd >= prd && wr >= pwr {
                        d.add("Read", Kind::Rate, ((rd - prd) as f64 / dt) as f32);
                        d.add("Write", Kind::Rate, ((wr - pwr) as f64 / dt) as f32);
                    }
                }
                drive.last = Some((rd, wr, now));
            }
            if let Some((used, total)) = self.mounts.get(&drive.id).and_then(|p| space(p)) {
                d.add("Space used", Kind::Level, used as f32 / total as f32 * 100.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_log() {
        let mut log = [0u8; 512];
        // 323 K, 100 % spare, 3 % used.
        log[1..3].copy_from_slice(&323u16.to_le_bytes());
        log[3] = 100;
        log[5] = 3;
        let (t, used) = parse_smart(&log);
        assert!((t.unwrap() - 49.85).abs() < 0.01);
        assert_eq!(used, Some(3.0));
        assert_eq!(parse_smart(&[0u8; 512]).0, None);
    }

    #[test]
    fn identify() {
        let mut id = vec![0u8; 4096];
        assert_eq!(identify_limits(&id), (None, None));
        id[266..268].copy_from_slice(&325u16.to_le_bytes());
        id[268..270].copy_from_slice(&327u16.to_le_bytes());
        let (w, c) = identify_limits(&id);
        assert!((w.unwrap() - 51.85).abs() < 0.01 && (c.unwrap() - 53.85).abs() < 0.01);
    }

    #[test]
    fn space_groups() {
        assert_eq!(space_group("/dev/disk3s1s1", "apfs").as_deref(), Some("disk3"));
        assert_eq!(space_group("/dev/disk3s5", "apfs").as_deref(), Some("disk3"));
        assert_eq!(space_group("/dev/disk12s2", "apfs").as_deref(), Some("disk12"));
        assert_eq!(space_group("/dev/disk4s1", "exfat").as_deref(), Some("disk4s1"));
        assert_eq!(space_group("map auto_home", "autofs"), None);
        assert_eq!(space_group("devfs", "devfs"), None);
    }

    #[test]
    fn names() {
        assert_eq!(bus_name("Apple Fabric", true), "Internal");
        assert_eq!(bus_name("USB", false), "USB");
        assert_eq!(fmt_decimal_size(500_277_792_768), "500 GB");
        assert_eq!(fmt_decimal_size(1_000_204_886_016), "1.0 TB");
    }
}
