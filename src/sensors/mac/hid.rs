//! Apple silicon's temperature sensors as the HID event system publishes
//! them. The API is private (every Mac hardware monitor uses it), so it's
//! looked up at run time and simply missing if a macOS release drops it.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use super::cf::{Cf, CFTypeRef, Library};

/// How often each sensor is read. Some take over a millisecond each, so
/// they're spread over the rounds instead of all read every second.
const REFRESH: Duration = Duration::from_secs(5);
const TEMPERATURE_EVENT: i64 = 15;

type ClientCreate = unsafe extern "C" fn(CFTypeRef) -> CFTypeRef;
type SetMatching = unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> i32;
type CopyServices = unsafe extern "C" fn(CFTypeRef) -> CFTypeRef;
type CopyProperty = unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> CFTypeRef;
type CopyEvent = unsafe extern "C" fn(CFTypeRef, i64, i32, i64) -> CFTypeRef;
type FloatValue = unsafe extern "C" fn(CFTypeRef, i32) -> f64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    /// CPU cluster sensors (M1 and M2).
    Cpu,
    Gpu,
    /// Die sensors elsewhere on the chip.
    Soc,
    /// Thermistors on the logic board.
    Board,
    Drive,
    Battery,
    /// Calibration references and anything unrecognised.
    Skip,
}

/// Where a sensor belongs, by the name the firmware gives it ("PMU tdie3",
/// "pACC MTR Temp Sensor2", "NAND CH0 temp", "gas gauge battery").
pub fn group(name: &str) -> Group {
    let pmu = name.starts_with("PMU");
    if ["pACC MTR", "eACC MTR", "PACC MTR", "EACC MTR"].iter().any(|p| name.starts_with(p)) {
        Group::Cpu
    } else if name.starts_with("GPU MTR") {
        Group::Gpu
    } else if ["SOC MTR", "ANE MTR", "ISP MTR", "PMGR SOC Die"].iter().any(|p| name.starts_with(p)) || (pmu && name.contains(" tdie")) {
        Group::Soc
    } else if pmu && name.contains(" tdev") {
        Group::Board
    } else if name.starts_with("NAND") {
        Group::Drive
    } else if name.starts_with("gas gauge battery") {
        Group::Battery
    } else {
        Group::Skip
    }
}

/// Sort key that puts "PMU tdev2" before "PMU tdev10" and "PMU2 tdev1" after both.
pub fn natural_key(name: &str) -> (String, u32) {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (stem, n) = name.split_at(name.len() - digits);
    (stem.to_string(), n.parse().unwrap_or(0))
}

pub struct Hid {
    _client: Cf,
    /// Keeps the service objects below alive.
    _array: Cf,
    services: Vec<CFTypeRef>,
    copy_event: CopyEvent,
    float_value: FloatValue,
    /// Name and group of each sensor, in the same order as `services`.
    pub sensors: Vec<(String, Group)>,
    /// Last value and when it's next due.
    cache: RefCell<Vec<(Option<f32>, Option<Instant>)>>,
}

impl Hid {
    pub fn open() -> Option<Hid> {
        let lib = Library::open("/System/Library/Frameworks/IOKit.framework/IOKit")?;
        let (create, set_matching, copy_services, copy_property, copy_event, float_value) = unsafe {
            (
                lib.get::<ClientCreate>("IOHIDEventSystemClientCreate")?,
                lib.get::<SetMatching>("IOHIDEventSystemClientSetMatching")?,
                lib.get::<CopyServices>("IOHIDEventSystemClientCopyServices")?,
                lib.get::<CopyProperty>("IOHIDServiceClientCopyProperty")?,
                lib.get::<CopyEvent>("IOHIDServiceClientCopyEvent")?,
                lib.get::<FloatValue>("IOHIDEventGetFloatValue")?,
            )
        };
        let client = unsafe { Cf::owned(create(std::ptr::null())) }?;
        // Vendor usage page 0xff00, usage 5: temperature sensors.
        let (page, usage) = (Cf::number(0xff00), Cf::number(5));
        let matching = Cf::dictionary(&[("PrimaryUsagePage", &page), ("PrimaryUsage", &usage)]);
        unsafe { set_matching(client.as_ptr(), matching.as_ptr()) };
        let array = unsafe { Cf::owned(copy_services(client.as_ptr())) }?;
        let services: Vec<CFTypeRef> = array.obj().items().iter().map(|s| s.as_ptr()).collect();
        let product = Cf::string("Product");
        let sensors: Vec<(String, Group)> = services
            .iter()
            .map(|&s| {
                let name = unsafe { Cf::owned(copy_property(s, product.as_ptr())) }.and_then(|n| n.obj().string()).unwrap_or_default();
                let g = group(&name);
                (name, g)
            })
            .collect();
        if sensors.is_empty() {
            return None;
        }
        let cache = RefCell::new(vec![(None, None); sensors.len()]);
        Some(Hid { _client: client, _array: array, services, copy_event, float_value, sensors, cache })
    }

    /// Indexes of the sensors in a group.
    pub fn in_group(&self, g: Group) -> Vec<usize> {
        self.sensors.iter().enumerate().filter(|(_, s)| s.1 == g).map(|(i, _)| i).collect()
    }

    /// The latest reading of sensor `i`, read again when it's due.
    pub fn value(&self, i: usize) -> Option<f32> {
        let mut cache = self.cache.borrow_mut();
        let (v, due) = cache.get_mut(i)?;
        let now = Instant::now();
        if due.is_none_or(|d| now >= d) {
            // After the first reading, stagger the sensors over the refresh
            // period so each round reads only a few of them.
            let next = if due.is_none() { REFRESH * (1 + i as u32 % 5) / 5 } else { REFRESH };
            *v = self.read(i);
            *due = Some(now + next);
        }
        *v
    }

    fn read(&self, i: usize) -> Option<f32> {
        let s = *self.services.get(i)?;
        let ev = unsafe { Cf::owned((self.copy_event)(s, TEMPERATURE_EVENT, 0, 0)) }?;
        let t = unsafe { (self.float_value)(ev.as_ptr(), (TEMPERATURE_EVENT << 16) as i32) } as f32;
        // Unconnected inputs read 0 or wildly negative.
        (t > -30.0 && t < 150.0 && t != 0.0).then_some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups() {
        assert_eq!(group("pACC MTR Temp Sensor2"), Group::Cpu);
        assert_eq!(group("eACC MTR Temp Sensor0"), Group::Cpu);
        assert_eq!(group("GPU MTR Temp Sensor1"), Group::Gpu);
        assert_eq!(group("SOC MTR Temp Sensor0"), Group::Soc);
        assert_eq!(group("PMGR SOC Die Temp Sensor1"), Group::Soc);
        assert_eq!(group("PMU tdie6"), Group::Soc);
        assert_eq!(group("PMU2 tdie10"), Group::Soc);
        assert_eq!(group("PMU tdev4"), Group::Board);
        assert_eq!(group("PMU2 tdev1"), Group::Board);
        assert_eq!(group("NAND CH0 temp"), Group::Drive);
        assert_eq!(group("gas gauge battery"), Group::Battery);
        assert_eq!(group("PMU tcal"), Group::Skip);
        assert_eq!(group(""), Group::Skip);
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["PMU2 tdev1", "PMU tdev10", "PMU tdev2"];
        v.sort_by_key(|n| natural_key(n));
        assert_eq!(v, ["PMU tdev2", "PMU tdev10", "PMU2 tdev1"]);
        assert_eq!(natural_key("NAND"), ("NAND".into(), 0));
    }
}
