//! Drives: temperature from the drive itself (Windows asks it through the
//! storage stack, no driver needed), activity and throughput from
//! performance counters, and space used across the drive's volumes.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeAta, BusTypeNvme, BusTypeRAID, BusTypeSata, BusTypeScsi, BusTypeSd, BusTypeUsb, CreateFileW,
    GetDiskFreeSpaceExW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty, StorageDeviceTemperatureProperty,
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_ID, STORAGE_PROPERTY_QUERY, STORAGE_TEMPERATURE_DATA_DESCRIPTOR,
    STORAGE_TEMPERATURE_INFO,
};

use super::pdh::Query;
use crate::sensors::{Class, Frame, Kind, Source};
use crate::winsys::wide;

/// Solid-state drives are asked for their temperature this often, hard
/// drives less often so Heft isn't the thing keeping them busy.
const SSD_TEMP_EVERY: Duration = Duration::from_secs(3);
const HDD_TEMP_EVERY: Duration = Duration::from_secs(60);
/// How often to look for drives that were plugged in or removed.
const RESCAN_EVERY: Duration = Duration::from_secs(15);

struct Drive {
    number: u32,
    name: String,
    detail: String,
    spinning: bool,
    /// SATA drives can also be asked through SMART (administrator only).
    sata: bool,
    /// Last temperatures (overall first), warning and critical levels.
    temps: Option<(Vec<f32>, Option<f32>, Option<f32>)>,
    temps_at: Option<Instant>,
    /// Temperature queries in a row that got nothing.
    misses: u8,
}

pub struct Drives {
    drives: Vec<Drive>,
    scanned: Instant,
    pdh: Option<Query>,
    idle: Option<usize>,
    read: Option<usize>,
    write: Option<usize>,
    admin: bool,
}

fn open(n: u32) -> Option<HANDLE> {
    let path = wide(format!(r"\\.\PhysicalDrive{n}"));
    // No access rights needed for these queries, so no administrator either.
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    (h != INVALID_HANDLE_VALUE).then_some(h)
}

/// IOCTL_STORAGE_QUERY_PROPERTY into a u64-aligned buffer.
fn property(h: HANDLE, id: STORAGE_PROPERTY_ID, bytes: usize) -> Option<Vec<u64>> {
    let q = STORAGE_PROPERTY_QUERY { PropertyId: id, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &q as *const _ as *const _,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr() as *mut _,
            (buf.len() * 8) as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    (ok != 0 && ret > 0).then_some(buf)
}

fn ascii_at(buf: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= buf.len() {
        return String::new();
    }
    let end = buf[off..].iter().position(|&c| c == 0).map(|e| off + e).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[off..end]).split_whitespace().collect::<Vec<_>>().join(" ")
}

#[allow(non_upper_case_globals)] // windows-sys names these like the C enum
fn describe(n: u32) -> Option<Drive> {
    let h = open(n)?;
    let d = (|| {
        let buf = property(h, StorageDeviceProperty, 1024)?;
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
        let desc = unsafe { &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
        let vendor = ascii_at(bytes, desc.VendorIdOffset);
        let product = ascii_at(bytes, desc.ProductIdOffset);
        let name = if vendor.is_empty() || product.starts_with(&vendor) || vendor.eq_ignore_ascii_case("nvme") {
            product
        } else {
            format!("{vendor} {product}")
        };
        let bus = match desc.BusType {
            BusTypeNvme => "NVMe",
            BusTypeSata | BusTypeAta => "SATA",
            BusTypeUsb => "USB",
            BusTypeRAID => "RAID",
            BusTypeScsi => "SCSI",
            BusTypeSd => "SD card",
            _ => "",
        };
        // Unknown when the drive (or its USB enclosure) doesn't say.
        let spinning = property(h, StorageDeviceSeekPenaltyProperty, 16)
            .map(|b| unsafe { (*(b.as_ptr() as *const DEVICE_SEEK_PENALTY_DESCRIPTOR)).IncursSeekPenalty });
        let mut geo: DISK_GEOMETRY_EX = unsafe { std::mem::zeroed() };
        let mut ret = 0u32;
        let size = unsafe {
            DeviceIoControl(
                h,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                std::ptr::null(),
                0,
                &mut geo as *mut _ as *mut _,
                std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        } != 0;
        let mut detail: Vec<String> = Vec::new();
        if !bus.is_empty() {
            detail.push(bus.into());
        }
        match spinning {
            Some(true) => detail.push("Hard drive".into()),
            Some(false) => detail.push("SSD".into()),
            None => {}
        }
        if size && geo.DiskSize > 0 {
            detail.push(fmt_decimal_size(geo.DiskSize as u64));
        }
        Some(Drive {
            number: n,
            name: if name.is_empty() { format!("Drive {n}") } else { name },
            detail: detail.join(" · "),
            spinning: spinning == Some(true),
            sata: bus == "SATA",
            temps: None,
            temps_at: None,
            misses: 0,
        })
    })();
    unsafe { CloseHandle(h) };
    d
}

/// Drive makers count in powers of 1000, so a "2 TB" drive says 2 TB here too.
fn fmt_decimal_size(bytes: u64) -> String {
    let tb = bytes as f64 / 1e12;
    if tb >= 1.0 { format!("{tb:.1} TB") } else { format!("{:.0} GB", bytes as f64 / 1e9) }
}

/// All temperature sensors the drive reports (the first is the overall one),
/// plus its warning and critical temperatures.
fn temperatures(n: u32) -> Option<(Vec<f32>, Option<f32>, Option<f32>)> {
    let h = open(n)?;
    let r = (|| {
        let buf = property(h, StorageDeviceTemperatureProperty, 512)?;
        let d = unsafe { &*(buf.as_ptr() as *const STORAGE_TEMPERATURE_DATA_DESCRIPTOR) };
        let count = (d.InfoCount as usize).min(8);
        let infos = unsafe { std::slice::from_raw_parts(d.TemperatureInfo.as_ptr() as *const STORAGE_TEMPERATURE_INFO, count) };
        // Drives report 0 or out-of-range values for sensors they don't have.
        let temps: Vec<f32> = infos.iter().map(|i| i.Temperature as f32).filter(|&t| t > 0.0 && t < 150.0).collect();
        if temps.is_empty() {
            return None;
        }
        let limit = |t: i16| (t > 20 && t < 150).then_some(t as f32);
        Some((temps, limit(d.WarningTemperature), limit(d.CriticalTemperature)))
    })();
    unsafe { CloseHandle(h) };
    r
}

/// Older SATA drives that don't answer the standard query still report
/// temperature in their SMART attributes (194, or 190 on some). Reading
/// SMART needs a read/write handle, which Windows only gives administrators.
fn smart_temperature(n: u32) -> Option<(Vec<f32>, Option<f32>, Option<f32>)> {
    use windows_sys::Win32::System::Ioctl::{
        IDEREGS, READ_ATTRIBUTES, READ_ATTRIBUTE_BUFFER_SIZE, SENDCMDINPARAMS, SMART_CMD, SMART_CYL_HI, SMART_CYL_LOW,
        SMART_RCV_DRIVE_DATA,
    };
    let path = wide(format!(r"\\.\PhysicalDrive{n}"));
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            0x8000_0000 | 0x4000_0000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut input: SENDCMDINPARAMS = unsafe { std::mem::zeroed() };
    input.cBufferSize = READ_ATTRIBUTE_BUFFER_SIZE;
    input.irDriveRegs = IDEREGS {
        bFeaturesReg: READ_ATTRIBUTES as u8,
        bSectorCountReg: 1,
        bSectorNumberReg: 1,
        bCylLowReg: SMART_CYL_LOW as u8,
        bCylHighReg: SMART_CYL_HI as u8,
        bDriveHeadReg: 0xA0,
        bCommandReg: SMART_CMD as u8,
        bReserved: 0,
    };
    input.bDriveNumber = n as u8;
    // SENDCMDOUTPARAMS header (16 bytes) followed by the 512-byte data block.
    const HEADER: usize = 16;
    let mut out = [0u8; HEADER + 512];
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            SMART_RCV_DRIVE_DATA,
            &input as *const _ as *const _,
            (std::mem::size_of::<SENDCMDINPARAMS>() - 1) as u32,
            out.as_mut_ptr() as *mut _,
            out.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    } != 0;
    unsafe { CloseHandle(h) };
    if !ok {
        return None;
    }
    smart_temp_from(&out[HEADER..]).map(|t| (vec![t], None, None))
}

/// Temperature from a SMART data block: 30 attributes of 12 bytes after a
/// 2-byte version; the current temperature is the attribute's first raw byte.
fn smart_temp_from(data: &[u8]) -> Option<f32> {
    let attrs: Vec<&[u8]> = data.get(2..2 + 30 * 12)?.chunks(12).collect();
    [194u8, 190].iter().find_map(|&id| {
        let a = attrs.iter().find(|a| a[0] == id)?;
        let t = a[5] as f32;
        (t > 0.0 && t < 100.0).then_some(t)
    })
}

fn scan() -> Vec<Drive> {
    (0..32).filter_map(describe).collect()
}

impl Drives {
    pub fn new() -> Drives {
        let mut pdh = Query::new();
        let (mut idle, mut read, mut write) = (None, None, None);
        if let Some(q) = pdh.as_mut() {
            idle = q.add(r"\PhysicalDisk(*)\% Idle Time");
            read = q.add(r"\PhysicalDisk(*)\Disk Read Bytes/sec");
            write = q.add(r"\PhysicalDisk(*)\Disk Write Bytes/sec");
            q.collect();
        }
        Drives { drives: scan(), scanned: Instant::now(), pdh, idle, read, write, admin: crate::platform::is_elevated() }
    }
}

/// "1 C: D:" to (1, ["C:", "D:"]).
fn parse_disk(name: &str) -> Option<(u32, Vec<&str>)> {
    let mut parts = name.split_whitespace();
    let n = parts.next()?.parse().ok()?;
    Some((n, parts.filter(|p| p.ends_with(':')).collect()))
}

fn space(letters: &[&str]) -> Option<(u64, u64)> {
    let mut total = 0u64;
    let mut free = 0u64;
    for l in letters {
        let root = wide(format!("{l}\\"));
        let (mut t, mut f) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut t, &mut f) } != 0 {
            total += t;
            free += f;
        }
    }
    (total > 0).then_some((total, free))
}

impl Source for Drives {
    fn sample(&mut self, frame: &mut Frame) {
        if self.scanned.elapsed() >= RESCAN_EVERY {
            let fresh = scan();
            // Keep cached temperatures for drives that are still here.
            let mut old = std::mem::take(&mut self.drives);
            self.drives = fresh
                .into_iter()
                .map(|d| match old.iter().position(|o| o.number == d.number && o.name == d.name) {
                    Some(i) => old.swap_remove(i),
                    None => d,
                })
                .collect();
            self.scanned = Instant::now();
        }

        let (mut idle, mut read, mut write) = (Vec::new(), Vec::new(), Vec::new());
        if let Some(q) = &self.pdh
            && q.collect()
        {
            idle = q.values(self.idle);
            read = q.values(self.read);
            write = q.values(self.write);
        }
        let find = |vals: &[(String, f64)], n: u32| {
            vals.iter().find(|(name, _)| parse_disk(name).map(|p| p.0) == Some(n)).map(|(name, v)| (name.clone(), *v))
        };

        for drive in &mut self.drives {
            let every = if drive.spinning { HDD_TEMP_EVERY } else { SSD_TEMP_EVERY };
            // A drive that keeps not answering (most USB enclosures) isn't asked again.
            if drive.misses < 3 && drive.temps_at.is_none_or(|t| t.elapsed() >= every) {
                drive.temps = temperatures(drive.number).or_else(|| if drive.sata && self.admin { smart_temperature(drive.number) } else { None });
                drive.temps_at = Some(Instant::now());
                drive.misses = if drive.temps.is_some() { 0 } else { drive.misses + 1 };
            }
            let d = frame.device(Class::Storage, &format!("disk{}", drive.number), &drive.name);
            d.detail(drive.detail.clone());
            if let Some((temps, warn, crit)) = &drive.temps {
                for (i, t) in temps.iter().enumerate() {
                    let label = if i == 0 { "Drive".to_string() } else { format!("Sensor #{i}") };
                    d.add(label, Kind::Temperature, *t).limits(*warn, *crit);
                }
            }
            if let Some((name, v)) = find(&idle, drive.number) {
                d.add("Activity", Kind::Load, (100.0 - v).clamp(0.0, 100.0) as f32);
                if let Some((_, letters)) = parse_disk(&name)
                    && let Some((total, free)) = space(&letters)
                {
                    d.add("Space used", Kind::Level, (total - free) as f32 / total as f32 * 100.0);
                }
            }
            if let Some((_, v)) = find(&read, drive.number) {
                d.add("Read", Kind::Rate, v.max(0.0) as f32);
            }
            if let Some((_, v)) = find(&write, drive.number) {
                d.add("Write", Kind::Rate, v.max(0.0) as f32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn disk_instances() {
        assert_eq!(super::parse_disk("1 C: D:"), Some((1, vec!["C:", "D:"])));
        assert_eq!(super::parse_disk("3"), Some((3, vec![])));
        assert_eq!(super::parse_disk("_Total"), None);
        assert_eq!(super::fmt_decimal_size(2_000_398_934_016), "2.0 TB");
    }

    #[test]
    fn smart_attributes() {
        let mut data = [0u8; 512];
        // Attribute 9 (power-on hours) first, then 194 (temperature) = 38 C.
        data[2] = 9;
        data[14] = 194;
        data[14 + 5] = 38;
        assert_eq!(super::smart_temp_from(&data), Some(38.0));
        assert_eq!(super::smart_temp_from(&[0u8; 512]), None);
    }
}
