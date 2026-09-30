//! The built-in battery, from its gas gauge through the AppleSmartBattery
//! driver: charge, capacity against what it was designed for (wear), cycle
//! count, voltage, current and charge or discharge power. Its temperature
//! comes from the SMC's battery sensors.

use std::rc::Rc;

use super::cf::IoObject;
use super::hid::{Group, Hid};
use super::smc::Smc;
use crate::sensors::{Class, Frame, Kind, Source};

/// The gas gauge counts charge in mAh. Watt-hours use each cell's nominal
/// voltage: 3.85 V is what Apple's recent packs are rated at (the M5
/// MacBook Pro's 6,249 mAh in three cells is sold as 72.4 Wh).
const CELL_VOLTS: f64 = 3.85;

/// What the driver reports, in its own units (mAh, mV, mA).
#[derive(Default)]
pub struct Raw {
    pub current: Option<i64>,
    pub max: Option<i64>,
    pub design: Option<i64>,
    pub nominal: Option<i64>,
    pub raw_max: Option<i64>,
    pub raw_current: Option<i64>,
    pub millivolts: Option<i64>,
    pub milliamps: Option<i64>,
    pub cells: Option<usize>,
}

pub fn readings(r: &Raw) -> Vec<(&'static str, Kind, f32)> {
    let mut out = Vec::new();
    // Apple silicon reports charge in percent (MaxCapacity 100), Intel Macs in mAh.
    let percent = r.max == Some(100);
    if let (Some(c), Some(m)) = (r.current, r.max)
        && m > 0
    {
        out.push(("Charge", Kind::Level, (c as f32 / m as f32 * 100.0).clamp(0.0, 100.0)));
    }
    let full = r.nominal.or(r.raw_max).or(r.max.filter(|_| !percent)).filter(|v| *v > 0);
    let remaining = r.raw_current.or(r.current.filter(|_| !percent));
    let design = r.design.filter(|v| *v > 0);
    if let Some(cells) = r.cells.filter(|c| *c > 0) {
        let wh = |mah: i64| (mah as f64 * cells as f64 * CELL_VOLTS / 1000.0) as f32;
        for (label, v) in [("Designed capacity", design), ("Full charge capacity", full), ("Remaining capacity", remaining)] {
            if let Some(v) = v {
                out.push((label, Kind::Energy, wh(v)));
            }
        }
    }
    if let (Some(d), Some(f)) = (design, full)
        && f <= d * 2
    {
        out.push(("Wear", Kind::Level, (100.0 - f as f32 / d as f32 * 100.0).max(0.0)));
    }
    if let Some(mv) = r.millivolts.filter(|v| *v > 0) {
        let volts = mv as f32 / 1000.0;
        out.push(("Battery", Kind::Voltage, volts));
        if let Some(ma) = r.milliamps.filter(|a| *a != 0) {
            let amps = ma as f32 / 1000.0;
            let (current, power) = if ma > 0 { ("Charge current", "Charge rate") } else { ("Discharge current", "Discharge rate") };
            out.push((current, Kind::Current, amps.abs()));
            out.push((power, Kind::Power, (amps * volts).abs()));
        }
    }
    out
}

pub struct Battery {
    obj: IoObject,
    cells: Option<usize>,
    smc: Option<Rc<Smc>>,
    /// SMC battery temperature keys this Mac has.
    temp_keys: Vec<&'static str>,
    hid: Option<(Rc<Hid>, Vec<usize>)>,
}

impl Battery {
    pub fn new(smc: Option<&Rc<Smc>>, hid: Option<&Rc<Hid>>) -> Option<Battery> {
        let obj = IoObject::first("AppleSmartBattery")?;
        if obj.property("BatteryInstalled").and_then(|b| b.obj().bool()) == Some(false) {
            return None;
        }
        let cells = obj
            .property("BatteryData")
            .and_then(|d| Some(d.obj().get("CellVoltage")?.items().len()))
            .filter(|n| *n > 0)
            .or_else(|| smc.and_then(|s| s.read("BNCB")).map(|n| n as usize).filter(|n| (1..=8).contains(n)));
        let temp_keys: Vec<&'static str> = smc.map(|s| ["TB0T", "TB1T", "TB2T"].into_iter().filter(|k| s.has(k)).collect()).unwrap_or_default();
        let hid = hid.map(|h| (h.clone(), h.in_group(Group::Battery))).filter(|(_, i)| !i.is_empty() && temp_keys.is_empty());
        Some(Battery { obj, cells, smc: smc.cloned(), temp_keys, hid })
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.obj.property(key).and_then(|v| v.obj().i64())
    }
}

impl Source for Battery {
    fn sample(&mut self, frame: &mut Frame) {
        let raw = Raw {
            current: self.int("CurrentCapacity"),
            max: self.int("MaxCapacity"),
            design: self.int("DesignCapacity"),
            nominal: self.int("NominalChargeCapacity"),
            raw_max: self.int("AppleRawMaxCapacity"),
            raw_current: self.int("AppleRawCurrentCapacity"),
            millivolts: self.int("Voltage"),
            milliamps: self.int("Amperage"),
            cells: self.cells,
        };
        let d = frame.device(Class::Battery, "battery", "Built-in battery");
        let mut detail = Vec::new();
        if let Some(c) = self.int("CycleCount").filter(|c| *c >= 0) {
            detail.push(format!("{} charge cycles", crate::util::fmt_count(c as u64)));
        }
        if self.int("ExternalConnected") == Some(1)
            && let Some(w) = self.obj.property("AdapterDetails").and_then(|a| a.obj().get("Watts")?.i64()).filter(|w| *w > 0)
        {
            detail.push(format!("{w} W power adapter"));
        }
        d.detail(detail.join(" · "));
        for (label, kind, v) in readings(&raw) {
            d.add(label, kind, v);
        }
        let temp = match (&self.smc, &self.hid) {
            (Some(smc), _) if !self.temp_keys.is_empty() => self.temp_keys.iter().filter_map(|k| smc.read(k)).filter(|t| *t > 0.0 && *t < 100.0).reduce(f32::max),
            (_, Some((hid, idx))) => idx.iter().filter_map(|&i| hid.value(i)).reduce(f32::max),
            _ => None,
        };
        if let Some(t) = temp {
            d.add("Battery", Kind::Temperature, t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(r: &[(&str, Kind, f32)], label: &str, kind: Kind) -> Option<f32> {
        r.iter().find(|x| x.0 == label && x.1 == kind).map(|x| x.2)
    }

    #[test]
    fn apple_silicon() {
        // An M5 MacBook Pro on its charger, full.
        let r = readings(&Raw {
            current: Some(100),
            max: Some(100),
            design: Some(6249),
            nominal: Some(6016),
            raw_max: Some(5864),
            raw_current: Some(5706),
            millivolts: Some(13006),
            milliamps: Some(0),
            cells: Some(3),
        });
        assert_eq!(find(&r, "Charge", Kind::Level), Some(100.0));
        let design = find(&r, "Designed capacity", Kind::Energy).unwrap();
        assert!((design - 72.18).abs() < 0.01, "{design}");
        let wear = find(&r, "Wear", Kind::Level).unwrap();
        assert!((wear - 3.73).abs() < 0.01, "{wear}");
        assert_eq!(find(&r, "Battery", Kind::Voltage), Some(13.006));
        assert!(!r.iter().any(|x| x.1 == Kind::Power || x.1 == Kind::Current));
    }

    #[test]
    fn intel_discharging() {
        // Intel Macs report capacities in mAh and no nominal capacity.
        let r = readings(&Raw {
            current: Some(4000),
            max: Some(8000),
            design: Some(8790),
            millivolts: Some(12000),
            milliamps: Some(-1500),
            cells: Some(3),
            ..Default::default()
        });
        assert_eq!(find(&r, "Charge", Kind::Level), Some(50.0));
        assert!((find(&r, "Remaining capacity", Kind::Energy).unwrap() - 46.2).abs() < 0.01);
        assert_eq!(find(&r, "Discharge current", Kind::Current), Some(1.5));
        assert_eq!(find(&r, "Discharge rate", Kind::Power), Some(18.0));
        assert!(find(&r, "Wear", Kind::Level).unwrap() > 8.9);
        // Without a cell count there are no watt-hours, rather than a guess.
        let r = readings(&Raw { current: Some(50), max: Some(100), design: Some(5000), nominal: Some(4000), ..Default::default() });
        assert!(!r.iter().any(|x| x.1 == Kind::Energy));
        assert_eq!(find(&r, "Wear", Kind::Level), Some(20.0));
    }
}
