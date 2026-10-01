//! The System Management Controller: fans, system power and most
//! temperatures. Read through the AppleSMC driver's user client, which any
//! user may open for reading; Heft never writes to it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use super::cf::{IoObject, IOConnectCallStructMethod, IOServiceClose, IOServiceOpen};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Vers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PLimit {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    mem: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyInfo {
    size: u32,
    kind: u32,
    attributes: u8,
}

/// The driver's one request and reply structure (SMCParamStruct).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Param {
    key: u32,
    vers: Vers,
    p_limit: PLimit,
    info: KeyInfo,
    result: u8,
    status: u8,
    command: u8,
    data32: u32,
    bytes: [u8; 32],
}

const _: () = assert!(std::mem::size_of::<Param>() == 80);

const HANDLE_EVENT: u32 = 2;
const READ_KEY: u8 = 5;
const KEY_AT_INDEX: u8 = 8;
const KEY_INFO: u8 = 9;

pub fn fourcc(s: &str) -> u32 {
    let b = s.as_bytes();
    (0..4).fold(0u32, |acc, i| acc << 8 | *b.get(i).unwrap_or(&b' ') as u32)
}

pub fn key_name(k: u32) -> String {
    k.to_be_bytes().iter().map(|&c| c as char).collect()
}

/// A number from an SMC value of the given type. Apple silicon stores
/// integers little-endian, Intel Macs big-endian; fixed-point types are
/// always big-endian and floats always little-endian.
pub fn decode(kind: &[u8; 4], b: &[u8], le_ints: bool) -> Option<f32> {
    let hex = |c: u8| (c as char).to_digit(16);
    let int = |n: usize| -> Option<u64> {
        let b = b.get(..n)?;
        Some(if le_ints {
            b.iter().rev().fold(0u64, |a, &x| a << 8 | x as u64)
        } else {
            b.iter().fold(0u64, |a, &x| a << 8 | x as u64)
        })
    };
    match kind {
        b"flt " => Some(f32::from_le_bytes(b.get(..4)?.try_into().ok()?)),
        b"ui8 " => int(1).map(|v| v as f32),
        b"ui16" => int(2).map(|v| v as f32),
        b"ui32" => int(4).map(|v| v as f32),
        b"si8 " => int(1).map(|v| v as u8 as i8 as f32),
        b"si16" => int(2).map(|v| v as u16 as i16 as f32),
        b"si32" => int(4).map(|v| v as u32 as i32 as f32),
        [b'f', b'p', i, f] => {
            hex(*i)?;
            let raw = u16::from_be_bytes(b.get(..2)?.try_into().ok()?);
            Some(raw as f32 / (1u32 << hex(*f)?) as f32)
        }
        [b's', b'p', i, f] => {
            hex(*i)?;
            let raw = i16::from_be_bytes(b.get(..2)?.try_into().ok()?);
            Some(raw as f32 / (1u32 << hex(*f)?) as f32)
        }
        _ => None,
    }
}

/// A key's type ("flt ", "ui16", "sp78") and size in bytes.
type Info = ([u8; 4], u32);

pub struct Smc {
    conn: u32,
    le_ints: bool,
    /// What's known about each key; `None` for keys this SMC doesn't have.
    info: RefCell<HashMap<u32, Option<Info>>>,
}

impl Smc {
    /// `apple_silicon` picks the byte order of integer values.
    pub fn open(apple_silicon: bool) -> Option<Smc> {
        let service = IoObject::first("AppleSMC")?;
        let mut conn = 0u32;
        if unsafe { IOServiceOpen(service.raw(), super::mach::task_self(), 0, &mut conn) } != 0 || conn == 0 {
            return None;
        }
        Some(Smc { conn, le_ints: apple_silicon, info: RefCell::new(HashMap::new()) })
    }

    fn call(&self, input: &Param) -> Option<Param> {
        let mut out = Param::default();
        let mut size = std::mem::size_of::<Param>();
        let r = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                HANDLE_EVENT,
                input as *const Param as *const c_void,
                std::mem::size_of::<Param>(),
                &mut out as *mut Param as *mut c_void,
                &mut size,
            )
        };
        (r == 0 && out.result == 0).then_some(out)
    }

    fn info(&self, key: u32) -> Option<Info> {
        if let Some(i) = self.info.borrow().get(&key) {
            return *i;
        }
        let i = self
            .call(&Param { key, command: KEY_INFO, ..Default::default() })
            .map(|p| (p.info.kind.to_be_bytes(), p.info.size))
            .filter(|(_, size)| *size > 0 && *size <= 32);
        self.info.borrow_mut().insert(key, i);
        i
    }

    /// Type and raw bytes of a key.
    pub fn raw(&self, key: &str) -> Option<([u8; 4], Vec<u8>)> {
        let k = fourcc(key);
        let (kind, size) = self.info(k)?;
        let p = self.call(&Param { key: k, info: KeyInfo { size, ..Default::default() }, command: READ_KEY, ..Default::default() })?;
        Some((kind, p.bytes[..size as usize].to_vec()))
    }

    pub fn read(&self, key: &str) -> Option<f32> {
        let (kind, bytes) = self.raw(key)?;
        decode(&kind, &bytes, self.le_ints).filter(|v| v.is_finite())
    }

    pub fn has(&self, key: &str) -> bool {
        self.info(fourcc(key)).is_some()
    }

    fn count(&self) -> u32 {
        self.raw("#KEY").and_then(|(_, b)| Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))).unwrap_or(0)
    }

    fn key_at(&self, i: u32) -> Option<u32> {
        self.call(&Param { data32: i, command: KEY_AT_INDEX, ..Default::default() }).map(|p| p.key)
    }

    /// Every key this SMC has, in its own (sorted) order.
    pub fn keys(&self) -> Vec<String> {
        (0..self.count()).map_while(|i| self.key_at(i)).map(key_name).collect()
    }

    /// Every key starting with `prefix` (up to four characters) whose value
    /// is a float. The SMC keeps its keys sorted, so this is a binary search
    /// rather than a walk through thousands of keys.
    pub fn float_keys(&self, prefix: &str) -> Vec<String> {
        let n = self.count();
        let want = prefix.as_bytes();
        // The prefix with the rest zeroed: the first key that could start with it.
        let target = fourcc(prefix) & !u32::MAX.checked_shr(8 * want.len() as u32).unwrap_or(0);
        let (mut lo, mut hi) = (0u32, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key_at(mid) {
                Some(k) if k < target => lo = mid + 1,
                Some(_) => hi = mid,
                None => return Vec::new(),
            }
        }
        let mut out = Vec::new();
        for i in lo..n {
            let Some(k) = self.key_at(i) else { break };
            let name = key_name(k);
            if !name.as_bytes().starts_with(want) {
                break;
            }
            if self.info(k).is_some_and(|(kind, size)| &kind == b"flt " && size == 4) {
                out.push(name);
            }
        }
        out
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe { IOServiceClose(self.conn) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(fourcc("TC0P"), 0x5443_3050);
        assert_eq!(key_name(0x5443_3050), "TC0P");
        assert_eq!(fourcc("ui8"), fourcc("ui8 "));
    }

    #[test]
    fn decodes_types() {
        // Apple silicon fan speed: float, little-endian (2502 RPM).
        assert_eq!(decode(b"flt ", &[0x00, 0x60, 0x1c, 0x45], true), Some(2502.0));
        // Intel fan speed: fpe2, big-endian, 2 fraction bits (1996 RPM).
        assert_eq!(decode(b"fpe2", &[0x1f, 0x30], false), Some(1996.0));
        // Intel temperature: sp78, 8 fraction bits (46.5 degrees).
        assert_eq!(decode(b"sp78", &[0x2e, 0x80], false), Some(46.5));
        assert_eq!(decode(b"sp78", &[0xff, 0x00], false), Some(-1.0));
        assert_eq!(decode(b"sp96", &[0x05, 0x00], false), Some(20.0));
        // Integers follow the platform's byte order.
        assert_eq!(decode(b"ui16", &[0x69, 0x18], true), Some(6249.0));
        assert_eq!(decode(b"ui16", &[0x18, 0x69], false), Some(6249.0));
        assert_eq!(decode(b"ui8 ", &[0x01], true), Some(1.0));
        assert_eq!(decode(b"si16", &[0xff, 0xfe], false), Some(-2.0));
        assert_eq!(decode(b"si8 ", &[0xff], true), Some(-1.0));
        // Unknown types and short values give nothing.
        assert_eq!(decode(b"hex_", &[0x01], true), None);
        assert_eq!(decode(b"flt ", &[0x00, 0x60], true), None);
        assert_eq!(decode(b"spxx", &[0x00, 0x60], true), None);
    }
}
