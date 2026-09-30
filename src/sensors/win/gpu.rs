//! Graphics cards. Windows' own graphics kernel reports temperature, fan
//! speed, clocks and power for any card whose driver supports it (the same
//! numbers Task Manager shows), and performance counters give per-engine
//! load and memory use. NVIDIA cards get more detail from NVML.

use std::collections::HashMap;

use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERINFO,
    D3DKMT_ADAPTERREGISTRYINFO, D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTER_PERFDATACAPS, D3DKMT_CLOSEADAPTER,
    D3DKMT_ENUMADAPTERS2, D3DKMT_NODE_PERFDATA, D3DKMT_QUERYADAPTERINFO, D3DKMT_SEGMENTSIZEINFO,
    KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_ADAPTERPERFDATA, KMTQAITYPE_ADAPTERPERFDATA_CAPS,
    KMTQAITYPE_ADAPTERREGISTRYINFO, KMTQAITYPE_ADAPTERTYPE, KMTQAITYPE_GETSEGMENTSIZE, KMTQAITYPE_NODEPERFDATA,
    KMTQUERYADAPTERINFOTYPE,
};

use super::nvml::Nvml;
use super::pdh::Query;
use crate::sensors::{Class, Frame, Kind, Source};
use crate::winsys::from_wide;

struct Adapter {
    handle: u32,
    /// "luid_0x00000000_0x0001AF02", as performance counters name it.
    luid: String,
    name: String,
    dedicated: u64,
    integrated: bool,
    max_fan: u32,
    /// Index into the NVML device list.
    nvml: Option<usize>,
}

pub struct Gpus {
    adapters: Vec<Adapter>,
    pdh: Option<Query>,
    engines: Option<usize>,
    dedicated: Option<usize>,
    shared: Option<usize>,
    nvml: Option<Nvml>,
}

fn query<T>(h: u32, kind: KMTQUERYADAPTERINFOTYPE, data: &mut T) -> bool {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: h,
        Type: kind,
        pPrivateDriverData: data as *mut T as *mut _,
        PrivateDriverDataSize: std::mem::size_of::<T>() as u32,
    };
    unsafe { D3DKMTQueryAdapterInfo(&mut q) >= 0 }
}

fn close(h: u32) {
    let c = D3DKMT_CLOSEADAPTER { hAdapter: h };
    unsafe { D3DKMTCloseAdapter(&c) };
}

impl Gpus {
    pub fn new() -> Gpus {
        let nvml = Nvml::load();
        let mut adapters = Vec::new();
        let mut seen_bus: Vec<u32> = Vec::new();
        let mut e = D3DKMT_ENUMADAPTERS2 { NumAdapters: 0, pAdapters: std::ptr::null_mut() };
        if unsafe { D3DKMTEnumAdapters2(&mut e) } >= 0 && e.NumAdapters > 0 {
            let mut list: Vec<D3DKMT_ADAPTERINFO> = vec![unsafe { std::mem::zeroed() }; e.NumAdapters as usize];
            e.pAdapters = list.as_mut_ptr();
            if unsafe { D3DKMTEnumAdapters2(&mut e) } >= 0 {
                list.truncate(e.NumAdapters as usize);
                for info in list {
                    let h = info.hAdapter;
                    // Bit 0: can render. Bit 2: software (Microsoft Basic Render Driver).
                    let mut kind = 0u32;
                    if !query(h, KMTQAITYPE_ADAPTERTYPE, &mut kind) || kind & 1 == 0 || kind & 4 != 0 {
                        close(h);
                        continue;
                    }
                    let mut addr = D3DKMT_ADAPTERADDRESS { BusNumber: u32::MAX, DeviceNumber: 0, FunctionNumber: 0 };
                    let bus = query(h, KMTQAITYPE_ADAPTERADDRESS, &mut addr).then_some(addr.BusNumber);
                    if let Some(b) = bus {
                        if seen_bus.contains(&b) {
                            close(h);
                            continue;
                        }
                        seen_bus.push(b);
                    }
                    let mut reg: D3DKMT_ADAPTERREGISTRYINFO = unsafe { std::mem::zeroed() };
                    let mut name = if query(h, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut reg) {
                        from_wide(&reg.AdapterString).trim().to_string()
                    } else {
                        String::new()
                    };
                    if name.is_empty() {
                        name = "Graphics adapter".into();
                    }
                    let mut seg = D3DKMT_SEGMENTSIZEINFO::default();
                    let dedicated = if query(h, KMTQAITYPE_GETSEGMENTSIZE, &mut seg) { seg.DedicatedVideoMemorySize } else { 0 };
                    let mut caps = D3DKMT_ADAPTER_PERFDATACAPS::default();
                    let max_fan = if query(h, KMTQAITYPE_ADAPTERPERFDATA_CAPS, &mut caps) { caps.MaxFanRPM } else { 0 };
                    let luid = format!(
                        "luid_0x{:08x}_0x{:08x}",
                        info.AdapterLuid.HighPart as u32, info.AdapterLuid.LowPart
                    );
                    let nvml_idx = nvml.as_ref().and_then(|n| n.devices.iter().position(|g| Some(g.bus) == bus));
                    adapters.push(Adapter {
                        handle: h,
                        luid,
                        name,
                        dedicated,
                        // Bit 5: the integrated half of a hybrid pair. Otherwise a
                        // small dedicated carve-out means it shares system memory.
                        integrated: kind & (1 << 5) != 0 || dedicated < (1 << 30),
                        max_fan,
                        nvml: nvml_idx,
                    });
                }
            }
        }
        let mut pdh = Query::new();
        let (mut engines, mut dedicated, mut shared) = (None, None, None);
        if let Some(q) = pdh.as_mut() {
            engines = q.add(r"\GPU Engine(*)\Utilization Percentage");
            dedicated = q.add(r"\GPU Adapter Memory(*)\Dedicated Usage");
            shared = q.add(r"\GPU Adapter Memory(*)\Shared Usage");
            q.collect();
        }
        Gpus { adapters, pdh, engines, dedicated, shared, nvml }
    }
}

impl Drop for Gpus {
    fn drop(&mut self) {
        for a in &self.adapters {
            close(a.handle);
        }
    }
}

/// "pid_1234_luid_0x00000000_0x0001AF02_phys_0_eng_3_engtype_VideoDecode"
/// to ("luid_0x00000000_0x0001af02", "VideoDecode").
fn parse_engine(name: &str) -> Option<(String, &str)> {
    let at = name.find("luid_")?;
    let luid = name.get(at..at + 26)?.to_ascii_lowercase();
    let ty = &name[name.find("engtype_")? + 8..];
    Some((luid, ty))
}

impl Source for Gpus {
    fn sample(&mut self, frame: &mut Frame) {
        // Engine load per adapter and engine type, summed over processes.
        let mut load: HashMap<(String, String), f64> = HashMap::new();
        let mut used: HashMap<String, (f64, f64)> = HashMap::new();
        if let Some(q) = &self.pdh
            && q.collect()
        {
            for (inst, v) in q.values(self.engines) {
                if let Some((luid, ty)) = parse_engine(&inst) {
                    // Compute_0, Compute_1 ... count as one kind of engine.
                    let ty = if ty.starts_with("Compute") { "Compute" } else { ty };
                    *load.entry((luid, ty.to_string())).or_default() += v;
                }
            }
            for (inst, v) in q.values(self.dedicated) {
                used.entry(inst.get(..26).unwrap_or_default().to_ascii_lowercase()).or_default().0 += v;
            }
            for (inst, v) in q.values(self.shared) {
                used.entry(inst.get(..26).unwrap_or_default().to_ascii_lowercase()).or_default().1 += v;
            }
        }

        for a in &self.adapters {
            let d = frame.device(Class::Gpu, &format!("gpu-{}", a.luid), &a.name);
            if a.integrated {
                d.detail("Integrated");
            }
            let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
            let perf = query(a.handle, KMTQAITYPE_ADAPTERPERFDATA, &mut perf).then_some(perf);
            if let (Some(i), Some(n)) = (a.nvml, &self.nvml) {
                n.sample(&n.devices[i], d);
            } else {
                if let Some(p) = &perf {
                    if p.Temperature > 0 {
                        d.add("Core", Kind::Temperature, p.Temperature as f32 / 10.0);
                    }
                    if p.MemoryFrequency > 0 {
                        d.add("Memory", Kind::Clock, (p.MemoryFrequency / 1_000_000) as f32);
                    }
                    if p.Power > 0 {
                        d.add("Board power", Kind::Level, p.Power as f32 / 10.0);
                    }
                }
                let mut node: D3DKMT_NODE_PERFDATA = unsafe { std::mem::zeroed() };
                if query(a.handle, KMTQAITYPE_NODEPERFDATA, &mut node) {
                    if node.Frequency > 0 {
                        d.add("Core", Kind::Clock, (node.Frequency / 1_000_000) as f32);
                    }
                    if node.Voltage > 0 {
                        d.add("Core", Kind::Voltage, node.Voltage as f32 / 1000.0);
                    }
                }
            }
            // Fan RPM comes from Windows for every vendor (NVML only gives percent).
            if let Some(p) = &perf
                && a.max_fan > 0
            {
                d.add("Fan", Kind::Fan, p.FanRPM as f32);
            }

            let engines: Vec<(&str, f64)> =
                load.iter().filter(|((l, _), _)| *l == a.luid).map(|((_, t), v)| (t.as_str(), v.min(100.0))).collect();
            if !engines.is_empty() && a.nvml.is_none() {
                let busiest = engines.iter().map(|e| e.1).fold(0.0, f64::max);
                d.add("Core", Kind::Load, busiest as f32);
            }
            for (ty, label) in [("3D", "3D"), ("Compute", "Compute"), ("Copy", "Copy"), ("VideoDecode", "Video decode"), ("VideoEncode", "Video encode")] {
                if let Some(&(_, v)) = engines.iter().find(|e| e.0 == ty) {
                    d.add(label, Kind::Load, v as f32);
                }
            }
            if a.nvml.is_none()
                && let Some(&(ded, sh)) = used.get(&a.luid)
            {
                if a.integrated {
                    d.add("Shared memory used", Kind::Data, sh as f32);
                } else {
                    d.add("Memory used", Kind::Data, ded as f32);
                    if a.dedicated > 0 {
                        d.add("Memory", Kind::Level, (ded / a.dedicated as f64 * 100.0) as f32);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn engine_names() {
        let (l, t) =
            super::parse_engine("pid_1234_luid_0x00000000_0x0001AF02_phys_0_eng_3_engtype_VideoDecode").unwrap();
        assert_eq!(l, "luid_0x00000000_0x0001af02");
        assert_eq!(t, "VideoDecode");
    }
}
