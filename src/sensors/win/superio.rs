//! The motherboard's sensor chip ("Super I/O"): fan speeds, fan drive,
//! voltages and board temperatures. Read through PawnIO's LpcIO module.
//!
//! Supported: the Nuvoton NCT6791D to NCT6799D and NCT6701D found on most
//! ASUS and ASRock boards of the last several years. Register layouts follow
//! the chips' datasheets as implemented by the Linux nct6775 driver and
//! LibreHardwareMonitor. What each pin is wired to depends on the board, so
//! boards Heft knows get real names ("CPU fan", "+12V"); others get the
//! chip's pin names with only the scaling the chip itself defines.

use super::pawnio::{BusMutex, Module, ISA_BUS};
use crate::sensors::{Class, Frame, Kind, Note, Source};

const CHIP_ID: u64 = 0x20;
const CHIP_REVISION: u64 = 0x21;
const DEVICE_SELECT: u64 = 0x07;
const ACTIVATE: u64 = 0x30;
const BASE_ADDRESS: u64 = 0x60;
const BASE_ADDRESS_ALT: u64 = 0x64;
const HWM_LDN: u64 = 0x0B;
const IO_SPACE_LOCK: u64 = 0x28;

const BANK_SELECT: u8 = 0x4E;
const VENDOR_ID_HIGH: u16 = 0x804F;
const VENDOR_ID_LOW: u16 = 0x004F;
const NUVOTON_VENDOR: u16 = 0x5CA3;
const VBAT_MONITOR_CONTROL: u16 = 0x005D;
const VBAT: usize = 8;

const VOLTAGES: [u16; 16] =
    [0x480, 0x481, 0x482, 0x483, 0x484, 0x485, 0x486, 0x487, 0x488, 0x489, 0x48A, 0x48B, 0x48C, 0x48D, 0x48E, 0x48F];
const FAN_COUNTS: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];

/// Each temperature source's own reading, in whole degrees.
const TEMPS: [(u16, &str); 9] = [
    (0x490, "System (SYSTIN)"),
    (0x491, "CPU socket (CPUTIN)"),
    (0x492, "AUXTIN0"),
    (0x493, "AUXTIN1"),
    (0x494, "AUXTIN2"),
    (0x495, "AUXTIN3"),
    (0x496, "AUXTIN4"),
    (0x4F4, "PECI/TSI 0"),
    (0x4F5, "PECI/TSI 1"),
];

#[derive(Clone, Copy)]
struct Chip {
    id: u16,
    name: &'static str,
    fans: usize,
    /// Fan PWM output registers, one per fan header.
    pwm: [u16; 7],
}

const PWM_A: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029];
const PWM_B: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];

const CHIPS: [Chip; 11] = [
    Chip { id: 0xC803, name: "Nuvoton NCT6791D", fans: 6, pwm: PWM_A },
    Chip { id: 0xC911, name: "Nuvoton NCT6792D", fans: 6, pwm: PWM_A },
    Chip { id: 0xC913, name: "Nuvoton NCT6792D-A", fans: 6, pwm: PWM_A },
    Chip { id: 0xD121, name: "Nuvoton NCT6793D", fans: 6, pwm: PWM_A },
    Chip { id: 0xD352, name: "Nuvoton NCT6795D", fans: 6, pwm: PWM_A },
    Chip { id: 0xD423, name: "Nuvoton NCT6796D", fans: 6, pwm: PWM_A },
    Chip { id: 0xD42A, name: "Nuvoton NCT6796D-R", fans: 7, pwm: PWM_A },
    Chip { id: 0xD451, name: "Nuvoton NCT6797D", fans: 7, pwm: PWM_B },
    Chip { id: 0xD42B, name: "Nuvoton NCT6798D", fans: 7, pwm: PWM_B },
    Chip { id: 0xD802, name: "Nuvoton NCT6799D", fans: 7, pwm: PWM_B },
    Chip { id: 0xD806, name: "Nuvoton NCT6701D", fans: 7, pwm: PWM_A },
];

/// How a known board wires the chip. Voltages are (input, name, Ri, Rf):
/// the board's divider, so volts = pin * (1 + Ri / Rf).
struct Profile {
    boards: &'static [&'static str],
    chip: u16,
    voltages: &'static [(usize, &'static str, f32, f32)],
    temps: &'static [(u16, &'static str)],
    fans: [&'static str; 7],
}

const PROFILES: &[Profile] = &[Profile {
    // Verified against LibreHardwareMonitor's mapping for this board.
    boards: &["ROG STRIX X870E-E GAMING WIFI"],
    chip: 0xD806,
    voltages: &[
        (0, "Vcore", 0.0, 1.0),
        (1, "+5V", 4.0, 1.0),
        (2, "AVSB", 34.0, 34.0),
        (3, "+3.3V", 34.0, 34.0),
        (4, "+12V", 11.0, 1.0),
        (7, "+3.3V standby", 34.0, 34.0),
        (8, "CMOS battery", 34.0, 34.0),
        (10, "CPU VDDIO / memory", 1.0, 1.0),
    ],
    temps: &[(0x490, "Motherboard"), (0x4F5, "CPU")],
    fans: ["Chassis fan 1", "CPU fan", "Chassis fan 2", "Chassis fan 3", "Chassis fan 4", "Chassis fan 5", "AIO pump"],
}];

/// On boards Heft doesn't know, only the inputs whose scaling the chip
/// itself defines get a name; the rest show the raw pin voltage.
const GENERIC_VOLTAGES: &[(usize, &str, f32, f32)] = &[
    (0, "CPU Vcore", 0.0, 1.0),
    (2, "AVSB", 34.0, 34.0),
    (3, "+3.3V", 34.0, 34.0),
    (7, "+3.3V standby", 34.0, 34.0),
    (8, "CMOS battery", 34.0, 34.0),
];

pub struct SuperIo {
    m: Module,
    isa: Option<BusMutex>,
    chip: Chip,
    base: u16,
    board: String,
    profile: Option<&'static Profile>,
    /// Fans that have spun at least once; headers with nothing on them stay hidden.
    spun: [bool; 7],
    bank: Option<u8>,
}

fn invalid_base(a: u16) -> bool {
    a < 0x100 || a & 0xF007 != 0
}

/// Find a supported chip. On failure, says what was found instead.
pub fn detect(m: Module, board: &str) -> Result<SuperIo, String> {
    let isa = BusMutex::new(ISA_BUS);
    let find = || -> Result<(Chip, u16), String> {
        let mut seen = String::new();
        for (slot, port) in [(0u64, 0x2Eu64), (1, 0x4E)] {
            if !m.run("ioctl_select_slot", &[slot]) {
                continue;
            }
            let outb = |p: u64, v: u64| m.run("ioctl_pio_outb", &[p, v]);
            let inb = |r: u64| m.get("ioctl_superio_inb", &[r]).unwrap_or(0xFF) as u8;
            let inw = |r: u64| m.get("ioctl_superio_inw", &[r]).unwrap_or(0xFFFF) as u16;
            let setb = |r: u64, v: u8| m.run("ioctl_superio_outb", &[r, v as u64]);
            // Winbond/Nuvoton "enter configuration mode" key.
            outb(port, 0x87);
            outb(port, 0x87);
            let id = ((inb(CHIP_ID) as u16) << 8) | inb(CHIP_REVISION) as u16;
            let exit = || outb(port, 0xAA);
            if id == 0 || id == 0xFFFF {
                exit();
                continue;
            }
            let Some(chip) = CHIPS.iter().find(|c| c.id == id).copied() else {
                exit();
                seen = format!("an unsupported sensor chip (ID {id:04X})");
                continue;
            };
            // Tells PawnIO which I/O ports belong to this chip.
            m.run("ioctl_find_bars", &[]);
            setb(DEVICE_SELECT, HWM_LDN as u8);
            // Some NCT6701D firmware leaves the monitoring block switched off.
            // Switching it on can move its ports, so PawnIO is told again.
            if chip.id == 0xD806 && inb(ACTIVATE) == 0 {
                setb(ACTIVATE, 1);
                m.run("ioctl_find_bars", &[]);
                setb(DEVICE_SELECT, HWM_LDN as u8);
            }
            let mut base = inw(BASE_ADDRESS);
            std::thread::sleep(std::time::Duration::from_millis(1));
            let mut verify = inw(BASE_ADDRESS);
            if chip.id == 0xD806 && base == verify && invalid_base(base) {
                base = inw(BASE_ADDRESS_ALT);
                std::thread::sleep(std::time::Duration::from_millis(1));
                verify = inw(BASE_ADDRESS_ALT);
            }
            // The I/O space lock hides the sensor registers until cleared.
            if base == verify {
                let lock = inb(IO_SPACE_LOCK);
                if lock & 0x10 != 0 {
                    setb(IO_SPACE_LOCK, lock & !0x10);
                }
            }
            exit();
            if base != verify || invalid_base(base) {
                seen = format!("a {} whose sensor registers couldn't be located", chip.name);
                continue;
            }
            return Ok((chip, base));
        }
        Err(if seen.is_empty() { "no supported sensor chip".into() } else { seen })
    };
    let (chip, base) = match &isa {
        Some(mx) => mx.with(200, find).unwrap_or_else(|| Err("another program kept the sensor chip busy".into()))?,
        None => find()?,
    };
    let profile = PROFILES.iter().find(|p| p.chip == chip.id && p.boards.iter().any(|b| board_matches(board, b)));
    let mut s = SuperIo { m, isa, chip, base, board: board.to_string(), profile, spun: [false; 7], bank: None };
    // The registers must answer. The NCT6701D doesn't report Nuvoton's
    // vendor ID there; every other supported chip does.
    let vendor = s.locked(|s| Some(((s.read(VENDOR_ID_HIGH)? as u16) << 8) | s.read(VENDOR_ID_LOW)? as u16)).flatten();
    match vendor {
        None => Err(format!("a {} whose sensor registers couldn't be read", chip.name)),
        Some(v) if chip.id != 0xD806 && v != NUVOTON_VENDOR => {
            Err(format!("a {} that didn't answer as expected", chip.name))
        }
        _ => Ok(s),
    }
}

/// "ASUSTeK COMPUTER INC. ROG STRIX X870E-E GAMING WIFI7 R2" matches
/// "ROG STRIX X870E-E GAMING WIFI".
fn board_matches(board: &str, known: &str) -> bool {
    board.to_ascii_uppercase().contains(&known.to_ascii_uppercase())
}

impl SuperIo {
    pub fn chip_name(&self) -> &'static str {
        self.chip.name
    }

    fn locked<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        self.bank = None;
        match self.isa.take() {
            Some(mx) => {
                let r = mx.with(50, || f(self));
                self.isa = Some(mx);
                r
            }
            None => Some(f(self)),
        }
    }

    /// One sensor register (bank in the high byte), or None if PawnIO
    /// refused the access. A failed read must never become a number.
    fn read(&mut self, addr: u16) -> Option<u8> {
        let (bank, reg) = ((addr >> 8) as u8, (addr & 0xFF) as u64);
        let a = (self.base + 5) as u64;
        let d = (self.base + 6) as u64;
        if self.bank != Some(bank) {
            self.bank = None;
            if !(self.m.run("ioctl_pio_outb", &[a, BANK_SELECT as u64]) && self.m.run("ioctl_pio_outb", &[d, bank as u64])) {
                return None;
            }
            self.bank = Some(bank);
        }
        if !self.m.run("ioctl_pio_outb", &[a, reg]) {
            return None;
        }
        self.m.get("ioctl_pio_inb", &[d]).map(|v| v as u8)
    }
}

fn decode_temp(raw: u8) -> Option<f32> {
    // Unconnected inputs read as 0, -128, -96 or 126/127. A board sensor is
    // never at or below freezing, so -1 and the like are junk too.
    let t = raw as i8;
    (!matches!(raw, 0x80 | 0xA0 | 0x7E | 0x7F) && (1..=115).contains(&t)).then_some(t as f32)
}

struct Readings {
    volts: [Option<u8>; 16],
    vbat_on: bool,
    temps: Vec<(u16, Option<u8>)>,
    counts: [Option<u16>; 7],
    pwm: [Option<u8>; 7],
}

impl Source for SuperIo {
    fn sample(&mut self, frame: &mut Frame) {
        let temp_regs: Vec<u16> = match self.profile {
            Some(p) => p.temps.iter().map(|t| t.0).collect(),
            None => TEMPS.iter().map(|t| t.0).collect(),
        };
        let fans = self.chip.fans;
        let pwm_regs = self.chip.pwm;
        let got = self.locked(|s| {
            let mut r = Readings { volts: [None; 16], vbat_on: false, temps: Vec::new(), counts: [None; 7], pwm: [None; 7] };
            for (i, reg) in VOLTAGES.iter().enumerate() {
                r.volts[i] = s.read(*reg);
            }
            r.vbat_on = s.read(VBAT_MONITOR_CONTROL).is_some_and(|v| v & 1 != 0);
            for reg in &temp_regs {
                r.temps.push((*reg, s.read(*reg)));
            }
            for i in 0..fans {
                let hi = s.read(FAN_COUNTS[i]);
                let lo = s.read(FAN_COUNTS[i] + 1);
                r.counts[i] = hi.zip(lo).map(|(hi, lo)| ((hi as u16) << 5) | (lo as u16 & 0x1F));
                r.pwm[i] = s.read(pwm_regs[i]);
            }
            r
        });
        let Some(r) = got else { return };

        let d = frame.device(Class::Motherboard, "board", &self.board);
        d.detail(self.chip.name);
        for (reg, raw) in &r.temps {
            let name = match self.profile {
                Some(p) => p.temps.iter().find(|t| t.0 == *reg).map(|t| t.1),
                None => TEMPS.iter().find(|t| t.0 == *reg).map(|t| t.1),
            };
            if let (Some(name), Some(t)) = (name, raw.and_then(decode_temp)) {
                d.add(name, Kind::Temperature, t);
            }
        }
        for i in 0..fans {
            let Some(count) = r.counts[i] else { continue };
            // A full count means the fan is stopped (or nothing is plugged in);
            // tiny counts are glitches.
            let rpm = if count >= 0x1FFF {
                0.0
            } else if count >= 0x15 {
                1.35e6 / count as f32
            } else {
                continue;
            };
            if rpm > 0.0 {
                self.spun[i] = true;
            }
            if !self.spun[i] {
                continue;
            }
            let name = match self.profile {
                Some(p) => p.fans[i].to_string(),
                None => format!("Fan #{}", i + 1),
            };
            d.add(name.clone(), Kind::Fan, rpm);
            if let Some(pwm) = r.pwm[i] {
                d.add(name, Kind::Duty, pwm as f32 / 2.55);
            }
        }
        let named = self.profile.map(|p| p.voltages).unwrap_or(GENERIC_VOLTAGES);
        for (i, raw) in r.volts.iter().enumerate() {
            let Some(raw) = raw else { continue };
            let pin = *raw as f32 * 0.008;
            if pin <= 0.0 || (i == VBAT && !r.vbat_on) {
                continue;
            }
            match named.iter().find(|v| v.0 == i) {
                Some(&(_, name, ri, rf)) => {
                    d.add(name, Kind::Voltage, pin * (1.0 + ri / rf));
                }
                // Known boards list every input that means something.
                None if self.profile.is_none() => {
                    d.add(format!("VIN{i}"), Kind::Voltage, pin);
                }
                None => {}
            }
        }
        if self.profile.is_none() {
            frame.note(Note::info(format!(
                "Heft doesn't know how this motherboard wires its {} yet, so fans are numbered and some voltages show the chip's raw pin readings.",
                self.chip.name
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn temps() {
        assert_eq!(super::decode_temp(35), Some(35.0));
        assert_eq!(super::decode_temp(0xFF), None);
        assert_eq!(super::decode_temp(0x80), None);
        assert_eq!(super::decode_temp(0x7F), None);
        assert_eq!(super::decode_temp(0), None);
    }

    #[test]
    fn boards() {
        assert!(super::board_matches("ASUSTeK COMPUTER INC. ROG STRIX X870E-E GAMING WIFI7 R2", "ROG STRIX X870E-E GAMING WIFI"));
        assert!(!super::board_matches("ROG STRIX X870-I GAMING WIFI", "ROG STRIX X870E-E GAMING WIFI"));
    }
}
