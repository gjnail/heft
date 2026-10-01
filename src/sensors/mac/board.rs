//! The Mac itself: fans and whole-system power from the SMC, and the
//! logic board's own temperature sensors.

use std::rc::Rc;

use super::cf::{text, IoObject};
use super::hid::{natural_key, Group, Hid};
use super::smc::Smc;
use super::sysctl_string;
use crate::sensors::{Class, Frame, Kind, Note, Source};

struct Fan {
    label: String,
    index: usize,
    max: Option<f32>,
}

pub struct Board {
    name: String,
    detail: String,
    smc: Rc<Smc>,
    fans: Vec<Fan>,
    /// SMC power keys this Mac has, as (key, label).
    power: Vec<(&'static str, &'static str)>,
    /// SMC temperature keys with a known meaning, as (key, label).
    temps: Vec<(&'static str, &'static str)>,
    /// HID thermistors, as (index, label).
    sensors: Vec<(usize, String)>,
    hid: Option<Rc<Hid>>,
}

/// "MacBook Pro (14-inch, M5)" where the device tree has it, else "Mac17,2".
pub(super) fn model_names() -> (String, String) {
    let model = sysctl_string("hw.model").unwrap_or_default();
    let product = IoObject::at_path("IODeviceTree:/product").and_then(|p| p.property("product-name")).and_then(|n| text(n.obj()));
    match product {
        Some(p) => (p, model),
        None if !model.is_empty() => (model, String::new()),
        None => ("Mac".into(), String::new()),
    }
}

/// Labels for the logic board's HID thermistors, numbered in their natural
/// order ("PMU tdev2" before "PMU tdev10", then the second PMU's).
pub fn sensor_labels(names: &[(usize, &str)]) -> Vec<(usize, String)> {
    let mut v: Vec<(usize, &str)> = names.to_vec();
    v.sort_by_key(|(_, n)| natural_key(n));
    v.into_iter().enumerate().map(|(i, (idx, _))| (idx, format!("Sensor #{}", i + 1))).collect()
}

impl Board {
    pub fn new(smc: Rc<Smc>, hid: Option<&Rc<Hid>>) -> Board {
        let (name, detail) = model_names();
        let count = smc.read("FNum").map(|n| n as usize).unwrap_or(0).min(8);
        let fans = (0..count)
            .filter(|i| smc.has(&format!("F{i}Ac")))
            .map(|i| Fan {
                label: if count == 1 { "Fan".into() } else { format!("Fan #{}", i + 1) },
                index: i,
                max: smc.read(&format!("F{i}Mx")).filter(|m| *m > 0.0),
            })
            .collect();
        let power = [("PSTR", "System total"), ("PDTR", "Power adapter")].into_iter().filter(|(k, _)| smc.has(k)).collect();
        // Only sensors whose meaning is documented; the SMC has dozens more.
        let temps = [("TA0P", "Ambient"), ("Tm0P", "Memory"), ("TW0P", "Wi-Fi"), ("Ts0P", "Palm rest")]
            .into_iter()
            .filter(|(k, _)| smc.has(k))
            .collect();
        // Inputs with nothing connected read nonsense; they're left out
        // before numbering so the numbers have no gaps.
        let sensors = hid
            .map(|h| {
                let names: Vec<(usize, &str)> =
                    h.in_group(Group::Board).into_iter().filter(|&i| h.value(i).is_some()).map(|i| (i, h.sensors[i].0.as_str())).collect();
                sensor_labels(&names)
            })
            .unwrap_or_default();
        Board { name, detail, smc, fans, power, temps, sensors, hid: hid.cloned() }
    }
}

impl Source for Board {
    fn sample(&mut self, frame: &mut Frame) {
        let mut readings: Vec<(String, Kind, f32)> = Vec::new();
        for (key, label) in &self.temps {
            if let Some(t) = self.smc.read(key).filter(|t| *t > 5.0 && *t < 130.0) {
                readings.push((label.to_string(), Kind::Temperature, t));
            }
        }
        if let Some(hid) = &self.hid {
            for (i, label) in &self.sensors {
                if let Some(t) = hid.value(*i) {
                    readings.push((label.clone(), Kind::Temperature, t));
                }
            }
        }
        for f in &self.fans {
            if let Some(rpm) = self.smc.read(&format!("F{}Ac", f.index)).filter(|r| *r >= 0.0) {
                readings.push((f.label.clone(), Kind::Fan, rpm));
                // How hard it's spinning, as a share of its top speed.
                if let Some(max) = f.max {
                    readings.push((f.label.clone(), Kind::Duty, (rpm / max * 100.0).clamp(0.0, 100.0)));
                }
            }
        }
        for (key, label) in &self.power {
            if let Some(w) = self.smc.read(key).filter(|w| *w >= 0.0) {
                readings.push((label.to_string(), Kind::Power, w));
            }
        }
        if self.fans.is_empty() {
            frame.note(Note::info("No fan speeds: this Mac has no fans."));
        }
        if readings.is_empty() {
            return;
        }
        let d = frame.device(Class::Motherboard, "board", &self.name);
        d.detail(self.detail.clone());
        for (label, kind, v) in readings {
            d.add(label, kind, v);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn thermistor_labels() {
        let names = [(7, "PMU2 tdev1"), (3, "PMU tdev10"), (5, "PMU tdev2")];
        let l = super::sensor_labels(&names);
        assert_eq!(l, [(5, "Sensor #1".to_string()), (3, "Sensor #2".to_string()), (7, "Sensor #3".to_string())]);
    }
}
