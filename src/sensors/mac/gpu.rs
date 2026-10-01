//! Graphics: load and memory from each GPU driver's own statistics (the
//! IOAccelerator "PerformanceStatistics" Activity Monitor's GPU history
//! uses), temperatures from the SMC on Apple silicon (or the driver on AMD
//! cards), and on Apple silicon clock and power from IOReport (soc.rs).

use std::rc::Rc;

use super::avg_max;
use super::cf::{text, IoObject, Obj};
use super::hid::Hid;
use super::smc::Smc;
use crate::sensors::{Class, Frame, Kind, Source};

pub struct Accelerator {
    obj: IoObject,
    pub key: String,
    pub name: String,
    detail: String,
    pub apple: bool,
}

/// Every graphics processor with a driver loaded.
pub fn accelerators() -> Vec<Accelerator> {
    IoObject::matching("IOAccelerator")
        .into_iter()
        .map(|obj| {
            let class = obj.property("IOClass").and_then(|c| c.obj().string()).unwrap_or_default();
            let model = obj
                .property("model")
                .and_then(|m| text(m.obj()))
                .or_else(|| obj.parent().and_then(|p| p.property("model")).and_then(|m| text(m.obj())))
                .unwrap_or_else(|| "Graphics".into());
            let apple = class.starts_with("AGX");
            let (key, name, detail) = if apple {
                let cores = obj.property("gpu-core-count").and_then(|c| c.obj().i64());
                ("gpu".to_string(), format!("{model} GPU"), cores.map(|n| format!("{n} cores")).unwrap_or_default())
            } else {
                let integrated = model.contains("Intel");
                (format!("gpu:{}", obj.id()), model, if integrated { "Integrated".into() } else { String::new() })
            };
            Accelerator { obj, key, name, detail, apple }
        })
        .collect()
}

/// Where the Apple GPU's temperatures come from.
pub enum Temps {
    Smc(Rc<Smc>, Vec<String>),
    Hid(Rc<Hid>, Vec<usize>),
    None,
}

pub struct Gpus {
    list: Vec<Accelerator>,
    temps: Temps,
}

impl Gpus {
    pub fn new(list: Vec<Accelerator>, temps: Temps) -> Gpus {
        Gpus { list, temps }
    }
}

/// The driver's statistics as readings. Keys differ by vendor: Apple and
/// Intel report utilization and shared memory, AMD also temperature, fan,
/// clocks, power and video memory.
pub fn stats_readings(s: Obj) -> Vec<(&'static str, Kind, f32)> {
    let num = |k: &str| s.get(k).and_then(|v| v.f64());
    let mut out = Vec::new();
    if let Some(v) = num("Device Utilization %").or_else(|| num("GPU Activity(%)")) {
        out.push(("Core", Kind::Load, v.clamp(0.0, 100.0) as f32));
    }
    for (key, label) in [("Renderer Utilization %", "Renderer"), ("Tiler Utilization %", "Tiler")] {
        if let Some(v) = num(key) {
            out.push((label, Kind::Load, v.clamp(0.0, 100.0) as f32));
        }
    }
    match (num("vramUsedBytes"), num("vramFreeBytes")) {
        (Some(used), free) => {
            out.push(("Memory used", Kind::Data, used as f32));
            if let Some(free) = free
                && used + free > 0.0
            {
                out.push(("Memory", Kind::Level, (used / (used + free) * 100.0) as f32));
            }
        }
        (None, _) => {
            if let Some(v) = num("In use system memory") {
                out.push(("Shared memory used", Kind::Data, v as f32));
            }
        }
    }
    let simple = [
        ("Temperature(C)", "Core", Kind::Temperature),
        ("Fan Speed(RPM)", "Fan", Kind::Fan),
        ("Fan Speed(%)", "Fan", Kind::Duty),
        ("Core Clock(MHz)", "Core", Kind::Clock),
        ("Memory Clock(MHz)", "Memory", Kind::Clock),
        ("Total Power(W)", "Board", Kind::Power),
    ];
    for (key, label, kind) in simple {
        if let Some(v) = num(key).filter(|v| *v > 0.0) {
            out.push((label, kind, v as f32));
        }
    }
    out
}

impl Source for Gpus {
    fn sample(&mut self, frame: &mut Frame) {
        for a in &self.list {
            let d = frame.device(Class::Gpu, &a.key, &a.name);
            d.detail(a.detail.clone());
            if a.apple {
                let vals: Vec<f32> = match &self.temps {
                    Temps::Smc(smc, keys) => keys.iter().filter_map(|k| smc.read(k)).collect(),
                    Temps::Hid(hid, idx) => idx.iter().filter_map(|&i| hid.value(i)).collect(),
                    Temps::None => Vec::new(),
                };
                let vals: Vec<f32> = vals.into_iter().filter(|t| *t > 5.0 && *t < 130.0).collect();
                if let Some((avg, max)) = avg_max(&vals) {
                    d.add("Core", Kind::Temperature, avg);
                    d.add("Hotspot", Kind::Temperature, max);
                }
            }
            if let Some(stats) = a.obj.property("PerformanceStatistics") {
                for (label, kind, v) in stats_readings(stats.obj()) {
                    d.add(label, kind, v);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::cf::Cf;

    #[test]
    fn apple_statistics() {
        let (dev, ren, mem) = (Cf::number(75), Cf::number(72), Cf::number(313_688_064));
        let s = Cf::dictionary(&[("Device Utilization %", &dev), ("Renderer Utilization %", &ren), ("In use system memory", &mem)]);
        let r = stats_readings(s.obj());
        assert_eq!(r[0], ("Core", Kind::Load, 75.0));
        assert_eq!(r[1], ("Renderer", Kind::Load, 72.0));
        assert_eq!(r[2], ("Shared memory used", Kind::Data, 313_688_064.0));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn amd_statistics() {
        let (act, used, free, temp, fan) = (Cf::number(40), Cf::number(1 << 30), Cf::number(3 << 30), Cf::number(61), Cf::number(0));
        let s = Cf::dictionary(&[
            ("GPU Activity(%)", &act),
            ("vramUsedBytes", &used),
            ("vramFreeBytes", &free),
            ("Temperature(C)", &temp),
            ("Fan Speed(RPM)", &fan),
        ]);
        let r = stats_readings(s.obj());
        assert!(r.contains(&("Core", Kind::Load, 40.0)));
        assert!(r.contains(&("Memory", Kind::Level, 25.0)));
        assert!(r.contains(&("Core", Kind::Temperature, 61.0)));
        // A fan that reads 0 on a card without one isn't shown.
        assert!(!r.iter().any(|x| x.1 == Kind::Fan));
    }
}
