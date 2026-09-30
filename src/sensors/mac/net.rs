//! Network adapters: download and upload speed of each connected Wi-Fi or
//! Ethernet port (not VPNs, bridges, AirDrop or other virtual interfaces).
//! Byte counters come from the interface MIB, which has the full 64-bit
//! values; getifaddrs and the routing socket hand ordinary processes only
//! the low 32 bits, which wrap every few seconds on a fast link.

use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::time::{Duration, Instant};

use super::cf::{Cf, CFTypeRef, Obj};
use crate::sensors::{Class, Frame, Kind, Source};

/// How often to look for adapters that were plugged in or removed.
const RESCAN_EVERY: Duration = Duration::from_secs(30);

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCNetworkInterfaceCopyAll() -> CFTypeRef;
    fn SCNetworkInterfaceGetBSDName(i: CFTypeRef) -> CFTypeRef;
    fn SCNetworkInterfaceGetLocalizedDisplayName(i: CFTypeRef) -> CFTypeRef;
    fn SCNetworkInterfaceGetInterfaceType(i: CFTypeRef) -> CFTypeRef;
}

/// Wi-Fi and Ethernet ports as (BSD name, the name System Settings shows),
/// e.g. ("en0", "Wi-Fi"), ("en9", "USB 10/100/1000 LAN").
fn ports() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(all) = (unsafe { Cf::owned(SCNetworkInterfaceCopyAll()) }) else { return out };
    let s = |p: CFTypeRef| unsafe { Obj::borrowed(p) }.and_then(|o| o.string());
    for i in all.obj().items() {
        let p = i.as_ptr();
        let (Some(bsd), Some(ty)) = (s(unsafe { SCNetworkInterfaceGetBSDName(p) }), s(unsafe { SCNetworkInterfaceGetInterfaceType(p) })) else {
            continue;
        };
        if ty == "Ethernet" || ty == "IEEE80211" {
            let name = s(unsafe { SCNetworkInterfaceGetLocalizedDisplayName(p) }).unwrap_or_else(|| bsd.clone());
            out.push((bsd, name));
        }
    }
    out.sort();
    out
}

/// struct ifmediareq, which <net/if.h> packs to 4 bytes.
#[repr(C, packed(4))]
struct IfMediaReq {
    name: [c_char; 16],
    current: i32,
    mask: i32,
    status: i32,
    active: i32,
    count: i32,
    list: *mut i32,
}

const _: () = assert!(std::mem::size_of::<IfMediaReq>() == 44);
/// _IOWR('i', 56, struct ifmediareq)
const SIOCGIFMEDIA: libc::c_ulong = 0xC02C_6938;
const IFM_AVALID: i32 = 1;
const IFM_ACTIVE: i32 = 2;

/// Whether the port has a link (a cable plugged in, or joined to Wi-Fi).
fn link_up(sock: i32, bsd: &str) -> bool {
    let mut req: IfMediaReq = unsafe { std::mem::zeroed() };
    for (d, s) in req.name.iter_mut().zip(bsd.bytes().take(15)) {
        *d = s as c_char;
    }
    if unsafe { libc::ioctl(sock, SIOCGIFMEDIA, &mut req) } != 0 {
        return false;
    }
    let status = req.status;
    status & IFM_AVALID != 0 && status & IFM_ACTIVE != 0
}

/// One interface's flags, bytes in and out, and link speed in bits per second.
#[derive(Debug, PartialEq)]
pub struct Counters {
    pub flags: u32,
    pub rx: u64,
    pub tx: u64,
    pub speed: u64,
}

/// A struct ifmibdata: name, five counts, four fillers, then if_data64.
pub fn parse_ifmib(b: &[u8]) -> Option<Counters> {
    let u32_at = |o: usize| Some(u32::from_ne_bytes(b.get(o..o + 4)?.try_into().ok()?));
    let u64_at = |o: usize| Some(u64::from_ne_bytes(b.get(o..o + 8)?.try_into().ok()?));
    const DATA: usize = 52;
    Some(Counters { flags: u32_at(20)?, speed: u64_at(DATA + 16)?, rx: u64_at(DATA + 64)?, tx: u64_at(DATA + 72)? })
}

/// net.link.generic.ifdata.<index>.general
fn counters(index: u32) -> Option<Counters> {
    const NETLINK_GENERIC: i32 = 0;
    const IFMIB_IFDATA: i32 = 2;
    const IFDATA_GENERAL: i32 = 1;
    let mut mib = [libc::CTL_NET, libc::PF_LINK, NETLINK_GENERIC, IFMIB_IFDATA, index as i32, IFDATA_GENERAL];
    let mut buf = [0u8; 256];
    let mut len = buf.len();
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 6, buf.as_mut_ptr() as *mut _, &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    parse_ifmib(&buf[..len])
}

fn fmt_link(bps: u64) -> String {
    if bps >= 1_000_000_000 {
        format!("{:.1} Gbps", bps as f64 / 1e9).replace(".0 ", " ")
    } else {
        format!("{} Mbps", bps / 1_000_000)
    }
}

pub struct Network {
    sock: i32,
    ports: Vec<(String, String)>,
    scanned: Instant,
    /// BSD name to (bytes in, bytes out) at the last sample.
    last: HashMap<String, (u64, u64)>,
    at: Instant,
}

impl Network {
    pub fn new() -> Network {
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        Network { sock, ports: ports(), scanned: Instant::now(), last: HashMap::new(), at: Instant::now() }
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        if self.sock >= 0 {
            unsafe { libc::close(self.sock) };
        }
    }
}

impl Source for Network {
    fn sample(&mut self, frame: &mut Frame) {
        if self.scanned.elapsed() >= RESCAN_EVERY {
            self.ports = ports();
            self.scanned = Instant::now();
        }
        let now = Instant::now();
        let dt = now.duration_since(self.at).as_secs_f64();
        let mut next = HashMap::new();
        for (bsd, port) in &self.ports {
            let Ok(c) = CString::new(bsd.as_str()) else { continue };
            let index = unsafe { libc::if_nametoindex(c.as_ptr()) };
            let Some(c) = (index > 0).then(|| counters(index)).flatten() else { continue };
            let up = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
            if c.flags & up != up || !link_up(self.sock, bsd) {
                continue;
            }
            next.insert(bsd.clone(), (c.rx, c.tx));
            let d = frame.device(Class::Network, &format!("net:{bsd}"), port);
            d.detail(if c.speed > 0 { format!("{bsd} · {}", fmt_link(c.speed)) } else { bsd.clone() });
            if let Some(&(rx, tx)) = self.last.get(bsd)
                && dt > 0.05
            {
                d.add("Download", Kind::Rate, (c.rx.saturating_sub(rx) as f64 / dt) as f32);
                d.add("Upload", Kind::Rate, (c.tx.saturating_sub(tx) as f64 / dt) as f32);
            }
        }
        self.last = next;
        self.at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_mib() {
        let mut b = vec![0u8; 180];
        b[..3].copy_from_slice(b"en9");
        b[20..24].copy_from_slice(&0x8863u32.to_ne_bytes());
        b[68..76].copy_from_slice(&1_000_000_000u64.to_ne_bytes());
        b[116..124].copy_from_slice(&32_480_874_982u64.to_ne_bytes());
        b[124..132].copy_from_slice(&23_788_366_596u64.to_ne_bytes());
        let c = parse_ifmib(&b).unwrap();
        assert_eq!(c, Counters { flags: 0x8863, rx: 32_480_874_982, tx: 23_788_366_596, speed: 1_000_000_000 });
        assert_eq!(parse_ifmib(&b[..100]), None);
    }

    #[test]
    fn link_speeds() {
        assert_eq!(fmt_link(1_000_000_000), "1 Gbps");
        assert_eq!(fmt_link(2_500_000_000), "2.5 Gbps");
        assert_eq!(fmt_link(100_000_000), "100 Mbps");
    }

    #[test]
    fn reads_loopback() {
        // Loopback is always there; its counters come back from the MIB.
        let c = CString::new("lo0").unwrap();
        let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
        assert!(counters(idx).is_some_and(|c| c.flags & libc::IFF_LOOPBACK as u32 != 0));
    }
}
