//! Laptop and UPS batteries, straight from the battery driver: charge,
//! capacity against what it was designed for (wear), voltage and charge rate.

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, GUID_DEVCLASS_BATTERY, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Power::{
    BatteryDeviceName, BatteryInformation, BatteryManufactureName, BATTERY_CHARGING, BATTERY_DISCHARGING,
    BATTERY_INFORMATION, BATTERY_QUERY_INFORMATION, BATTERY_STATUS, BATTERY_UNKNOWN_CAPACITY, BATTERY_UNKNOWN_RATE,
    BATTERY_UNKNOWN_VOLTAGE, BATTERY_WAIT_STATUS, IOCTL_BATTERY_QUERY_INFORMATION, IOCTL_BATTERY_QUERY_STATUS,
    IOCTL_BATTERY_QUERY_TAG,
};

use crate::sensors::{Class, Frame, Kind, Source};
use crate::winsys::from_wide;

pub struct Batteries {
    paths: Vec<Vec<u16>>,
}

impl Batteries {
    pub fn new() -> Batteries {
        Batteries { paths: device_paths() }
    }
}

/// Device paths of every battery Windows knows about.
fn device_paths() -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVCLASS_BATTERY, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set == -1 {
            return out;
        }
        for i in 0..8 {
            let mut did: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &GUID_DEVCLASS_BATTERY, i, &mut did) == 0 {
                break;
            }
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &did, std::ptr::null_mut(), 0, &mut need, std::ptr::null_mut());
            if need == 0 {
                continue;
            }
            let mut buf = vec![0u64; (need as usize).div_ceil(8)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, detail, need, std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                let chars = (need as usize - 4) / 2;
                let path = std::slice::from_raw_parts((*detail).DevicePath.as_ptr(), chars);
                let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                let mut p = path[..end].to_vec();
                p.push(0);
                out.push(p);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn ioctl<I, O>(h: HANDLE, code: u32, input: &I, out: &mut O) -> bool {
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            code,
            input as *const I as *const _,
            std::mem::size_of::<I>() as u32,
            out as *mut O as *mut _,
            std::mem::size_of::<O>() as u32,
            &mut ret,
            std::ptr::null_mut(),
        ) != 0
    }
}

fn read_string(h: HANDLE, tag: u32, level: i32) -> String {
    let q = BATTERY_QUERY_INFORMATION { BatteryTag: tag, InformationLevel: level, AtRate: 0 };
    let mut buf = [0u16; 128];
    if ioctl(h, IOCTL_BATTERY_QUERY_INFORMATION, &q, &mut buf) { from_wide(&buf).trim().to_string() } else { String::new() }
}

impl Source for Batteries {
    fn sample(&mut self, frame: &mut Frame) {
        for (idx, path) in self.paths.iter().enumerate() {
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
                continue;
            }
            let wait = 0u32;
            let mut tag = 0u32;
            if ioctl(h, IOCTL_BATTERY_QUERY_TAG, &wait, &mut tag) && tag != 0 {
                read_battery(h, tag, idx, frame);
            }
            unsafe { CloseHandle(h) };
        }
    }
}

fn read_battery(h: HANDLE, tag: u32, idx: usize, frame: &mut Frame) {
    let q = BATTERY_QUERY_INFORMATION { BatteryTag: tag, InformationLevel: BatteryInformation, AtRate: 0 };
    let mut info: BATTERY_INFORMATION = unsafe { std::mem::zeroed() };
    if !ioctl(h, IOCTL_BATTERY_QUERY_INFORMATION, &q, &mut info) {
        return;
    }
    // Capacities are in mWh unless the battery reports relative units.
    const RELATIVE: u32 = 0x4000_0000;
    let relative = info.Capabilities & RELATIVE != 0;
    let name = read_string(h, tag, BatteryDeviceName);
    let maker = read_string(h, tag, BatteryManufactureName);
    let title = match (maker.is_empty(), name.is_empty()) {
        (false, false) if !name.starts_with(&maker) => format!("{maker} {name}"),
        (_, false) => name,
        _ => format!("Battery {}", idx + 1),
    };
    let d = frame.device(Class::Battery, &format!("battery{idx}"), &title);
    let chem = String::from_utf8_lossy(&info.Chemistry).trim_end_matches('\0').trim().to_string();
    let mut detail = Vec::new();
    if !chem.is_empty() {
        detail.push(chem);
    }
    if info.CycleCount > 0 {
        detail.push(format!("{} charge cycles", info.CycleCount));
    }
    d.detail(detail.join(" · "));

    let full = info.FullChargedCapacity;
    let design = info.DesignedCapacity;
    if !relative {
        if design > 0 {
            d.add("Designed capacity", Kind::Energy, design as f32 / 1000.0);
        }
        if full > 0 {
            d.add("Full charge capacity", Kind::Energy, full as f32 / 1000.0);
        }
    }
    if design > 0 && full > 0 && full <= design * 2 {
        d.add("Wear", Kind::Level, (100.0 - full as f32 / design as f32 * 100.0).max(0.0));
    }

    let ws = BATTERY_WAIT_STATUS { BatteryTag: tag, Timeout: 0, PowerState: 0, LowCapacity: 0, HighCapacity: 0 };
    let mut st = BATTERY_STATUS { PowerState: 0, Capacity: 0, Voltage: 0, Rate: 0 };
    if !ioctl(h, IOCTL_BATTERY_QUERY_STATUS, &ws, &mut st) {
        return;
    }
    if st.Capacity != BATTERY_UNKNOWN_CAPACITY {
        if !relative {
            d.add("Remaining capacity", Kind::Energy, st.Capacity as f32 / 1000.0);
        }
        if full > 0 {
            d.add("Charge", Kind::Level, (st.Capacity as f32 / full as f32 * 100.0).min(100.0));
        }
    }
    if st.Voltage != BATTERY_UNKNOWN_VOLTAGE && st.Voltage > 0 {
        d.add("Battery", Kind::Voltage, st.Voltage as f32 / 1000.0);
    }
    if st.Rate as u32 != BATTERY_UNKNOWN_RATE && !relative {
        let w = st.Rate as f32 / 1000.0;
        if st.PowerState & BATTERY_CHARGING != 0 {
            d.add("Charge rate", Kind::Power, w.abs());
        } else if st.PowerState & BATTERY_DISCHARGING != 0 {
            d.add("Discharge rate", Kind::Power, w.abs());
        }
    }
}
