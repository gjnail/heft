//! Network adapters: download and upload speed of each connected physical
//! adapter (not VPNs, virtual switches or filter layers).

use std::collections::HashMap;
use std::time::Instant;

use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};

use crate::sensors::{Class, Frame, Kind, Source};
use crate::winsys::from_wide;

const IF_OPER_STATUS_UP: i32 = 1;

pub struct Network {
    /// Interface LUID to (bytes in, bytes out) at the last sample.
    last: HashMap<u64, (u64, u64)>,
    at: Instant,
}

impl Network {
    pub fn new() -> Network {
        Network { last: HashMap::new(), at: Instant::now() }
    }
}

fn fmt_link(bps: u64) -> String {
    if bps >= 1_000_000_000 {
        format!("{:.1} Gbps", bps as f64 / 1e9).replace(".0 ", " ")
    } else {
        format!("{} Mbps", bps / 1_000_000)
    }
}

impl Source for Network {
    fn sample(&mut self, frame: &mut Frame) {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.at).as_secs_f64();
        let rows = unsafe { std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
        let mut next = HashMap::new();
        for r in rows {
            let flags = r.InterfaceAndOperStatusFlags._bitfield;
            // HardwareInterface set, FilterInterface clear.
            let physical = flags & 1 != 0 && flags & 2 == 0;
            if !physical || r.OperStatus != IF_OPER_STATUS_UP {
                continue;
            }
            let luid = unsafe { r.InterfaceLuid.Value };
            next.insert(luid, (r.InOctets, r.OutOctets));
            let alias = from_wide(&r.Alias);
            let desc = from_wide(&r.Description);
            let d = frame.device(Class::Network, &format!("net-{luid:x}"), if alias.is_empty() { &desc } else { &alias });
            let speed = r.ReceiveLinkSpeed.max(r.TransmitLinkSpeed);
            d.detail(if speed > 0 { format!("{desc} · {}", fmt_link(speed)) } else { desc });
            if let Some(&(i, o)) = self.last.get(&luid)
                && dt > 0.05
            {
                d.add("Download", Kind::Rate, (r.InOctets.saturating_sub(i) as f64 / dt) as f32);
                d.add("Upload", Kind::Rate, (r.OutOctets.saturating_sub(o) as f64 / dt) as f32);
            }
        }
        unsafe { FreeMibTable(table as *const _) };
        self.last = next;
        self.at = now;
    }
}
