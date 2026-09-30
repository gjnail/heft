//! NVIDIA Management Library, which ships with the NVIDIA driver. Loaded at
//! run time, so Heft works the same without it.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use crate::sensors::{FrameDevice, Kind};
use crate::winsys::wide;

type Dev = *mut c_void;
type Ret = u32;

#[repr(C)]
struct PciInfo {
    bus_id_legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_subsystem_id: u32,
    bus_id: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

const TEMPERATURE_GPU: u32 = 0;
const CLOCK_GRAPHICS: u32 = 0;
const CLOCK_MEM: u32 = 2;
const CLOCK_VIDEO: u32 = 3;

type FnVoid = unsafe extern "C" fn() -> Ret;
type FnU32 = unsafe extern "C" fn(Dev, *mut u32) -> Ret;
type FnArgU32 = unsafe extern "C" fn(Dev, u32, *mut u32) -> Ret;

struct Api {
    shutdown: FnVoid,
    temperature: Option<FnArgU32>,
    fan: Option<FnU32>,
    num_fans: Option<FnU32>,
    fan_v2: Option<FnArgU32>,
    power: Option<FnU32>,
    power_limit: Option<FnU32>,
    clock: Option<FnArgU32>,
    utilization: Option<unsafe extern "C" fn(Dev, *mut Utilization) -> Ret>,
    memory: Option<unsafe extern "C" fn(Dev, *mut Memory) -> Ret>,
    encoder: Option<unsafe extern "C" fn(Dev, *mut u32, *mut u32) -> Ret>,
    decoder: Option<unsafe extern "C" fn(Dev, *mut u32, *mut u32) -> Ret>,
}

pub struct Nvml {
    lib: HMODULE,
    api: Api,
    pub devices: Vec<Gpu>,
}

pub struct Gpu {
    dev: Dev,
    /// PCI bus number, to match this card with its Windows adapter.
    pub bus: u32,
}

// SAFETY: NVML is thread-safe, and Heft only calls it from the sampling thread.
unsafe impl Send for Nvml {}

impl Nvml {
    pub fn load() -> Option<Nvml> {
        let lib = [
            wide("nvml.dll"),
            wide(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll"),
        ]
        .iter()
        .enumerate()
        .map(|(i, p)| unsafe {
            LoadLibraryExW(p.as_ptr(), std::ptr::null_mut(), if i == 0 { LOAD_LIBRARY_SEARCH_SYSTEM32 } else { 0 })
        })
        .find(|h| !h.is_null())?;

        macro_rules! sym {
            ($name:literal) => {
                unsafe { GetProcAddress(lib, concat!($name, "\0").as_ptr()).map(|f| std::mem::transmute(f)) }
            };
        }
        let init: Option<FnVoid> = sym!("nvmlInit_v2");
        let shutdown: Option<FnVoid> = sym!("nvmlShutdown");
        let count: Option<unsafe extern "C" fn(*mut u32) -> Ret> = sym!("nvmlDeviceGetCount_v2");
        let handle: Option<unsafe extern "C" fn(u32, *mut Dev) -> Ret> = sym!("nvmlDeviceGetHandleByIndex_v2");

        let pci: Option<unsafe extern "C" fn(Dev, *mut PciInfo) -> Ret> =
            sym!("nvmlDeviceGetPciInfo_v3").or(sym!("nvmlDeviceGetPciInfo_v2"));
        let (Some(init), Some(shutdown), Some(count), Some(handle), Some(pci)) =
            (init, shutdown, count, handle, pci)
        else {
            unsafe { FreeLibrary(lib) };
            return None;
        };
        if unsafe { init() } != 0 {
            unsafe { FreeLibrary(lib) };
            return None;
        }
        let api = Api {
            shutdown,
            temperature: sym!("nvmlDeviceGetTemperature"),
            fan: sym!("nvmlDeviceGetFanSpeed"),
            num_fans: sym!("nvmlDeviceGetNumFans"),
            fan_v2: sym!("nvmlDeviceGetFanSpeed_v2"),
            power: sym!("nvmlDeviceGetPowerUsage"),
            power_limit: sym!("nvmlDeviceGetEnforcedPowerLimit"),
            clock: sym!("nvmlDeviceGetClockInfo"),
            utilization: sym!("nvmlDeviceGetUtilizationRates"),
            memory: sym!("nvmlDeviceGetMemoryInfo"),
            encoder: sym!("nvmlDeviceGetEncoderUtilization"),
            decoder: sym!("nvmlDeviceGetDecoderUtilization"),
        };

        let mut devices = Vec::new();
        let mut n = 0u32;
        if unsafe { count(&mut n) } == 0 {
            for i in 0..n {
                let mut dev: Dev = std::ptr::null_mut();
                if unsafe { handle(i, &mut dev) } != 0 {
                    continue;
                }
                let mut info: PciInfo = unsafe { std::mem::zeroed() };
                let bus = if unsafe { pci(dev, &mut info) } == 0 { info.bus } else { u32::MAX };
                devices.push(Gpu { dev, bus });
            }
        }
        Some(Nvml { lib, api, devices })
    }

    /// Everything NVML knows about one card.
    pub fn sample(&self, gpu: &Gpu, d: &mut FrameDevice) {
        let a = &self.api;
        let dev = gpu.dev;
        let get = |f: Option<FnU32>| -> Option<u32> {
            let mut v = 0u32;
            (unsafe { f?(dev, &mut v) } == 0).then_some(v)
        };
        let get_arg = |f: Option<FnArgU32>, arg: u32| -> Option<u32> {
            let mut v = 0u32;
            (unsafe { f?(dev, arg, &mut v) } == 0).then_some(v)
        };
        if let Some(t) = get_arg(a.temperature, TEMPERATURE_GPU) {
            d.add("Core", Kind::Temperature, t as f32);
        }
        if let Some(mw) = get(a.power) {
            d.add("Board", Kind::Power, mw as f32 / 1000.0);
            if let Some(limit) = get(a.power_limit).filter(|&l| l > 0) {
                d.add("Board power", Kind::Level, mw as f32 / limit as f32 * 100.0);
            }
        }
        if let Some(mhz) = get_arg(a.clock, CLOCK_GRAPHICS) {
            d.add("Core", Kind::Clock, mhz as f32);
        }
        if let Some(mhz) = get_arg(a.clock, CLOCK_MEM) {
            d.add("Memory", Kind::Clock, mhz as f32);
        }
        if let Some(mhz) = get_arg(a.clock, CLOCK_VIDEO) {
            d.add("Video", Kind::Clock, mhz as f32);
        }
        let fans = get(a.num_fans).unwrap_or(0);
        if fans > 1 {
            for i in 0..fans {
                if let Some(p) = get_arg(a.fan_v2, i) {
                    d.add(format!("Fan #{}", i + 1), Kind::Duty, p as f32);
                }
            }
        } else if let Some(p) = get(a.fan) {
            d.add("Fan", Kind::Duty, p as f32);
        }
        if let Some(f) = a.utilization {
            let mut u = Utilization::default();
            if unsafe { f(dev, &mut u) } == 0 {
                d.add("Core", Kind::Load, u.gpu as f32);
                d.add("Memory controller", Kind::Load, u.memory as f32);
            }
        }
        for (label, f) in [("Video encode", a.encoder), ("Video decode", a.decoder)] {
            let (mut util, mut period) = (0u32, 0u32);
            if let Some(f) = f
                && unsafe { f(dev, &mut util, &mut period) } == 0
            {
                d.add(label, Kind::Load, util as f32);
            }
        }
        if let Some(f) = a.memory {
            let mut m = Memory::default();
            if unsafe { f(dev, &mut m) } == 0 && m.total > 0 {
                d.add("Memory used", Kind::Data, m.used as f32);
                d.add("Memory", Kind::Level, m.used as f32 / m.total as f32 * 100.0);
            }
        }
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            (self.api.shutdown)();
            FreeLibrary(self.lib);
        }
    }
}
