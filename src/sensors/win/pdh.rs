//! Performance counters (the numbers Task Manager and perfmon show).

use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};

use super::from_wide_ptr;
use crate::winsys::wide;

pub struct Query {
    q: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
}

// SAFETY: a query is only ever used by the thread that owns it; the handles
// are plain PDH handles with no thread affinity.
unsafe impl Send for Query {}

impl Query {
    pub fn new() -> Option<Query> {
        let mut q: PDH_HQUERY = std::ptr::null_mut();
        let r = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut q) };
        (r == 0).then_some(Query { q, counters: Vec::new() })
    }

    /// Add a counter by its English path, e.g. `\Processor Information(*)\% Processor Time`.
    /// Returns its index for [`Query::values`], or None if this system doesn't have it.
    pub fn add(&mut self, path: &str) -> Option<usize> {
        let w = wide(path);
        let mut c: PDH_HCOUNTER = std::ptr::null_mut();
        let r = unsafe { PdhAddEnglishCounterW(self.q, w.as_ptr(), 0, &mut c) };
        if r != 0 {
            return None;
        }
        self.counters.push(c);
        Some(self.counters.len() - 1)
    }

    pub fn collect(&self) -> bool {
        unsafe { PdhCollectQueryData(self.q) == 0 }
    }

    /// Every instance of a counter as (instance name, value), from the last
    /// two collections. Empty until the query has been collected twice.
    pub fn values(&self, idx: Option<usize>) -> Vec<(String, f64)> {
        let Some(&c) = idx.and_then(|i| self.counters.get(i)) else { return Vec::new() };
        // PDH_FMT_NOCAP100: percentages above 100 (turbo clocks) are kept.
        let fmt = PDH_FMT_DOUBLE | 0x8000;
        let mut bytes = 0u32;
        let mut count = 0u32;
        let r = unsafe { PdhGetFormattedCounterArrayW(c, fmt, &mut bytes, &mut count, std::ptr::null_mut()) };
        if r != PDH_MORE_DATA || bytes == 0 {
            return Vec::new();
        }
        // u64 elements keep the item array 8-byte aligned.
        let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        let r = unsafe { PdhGetFormattedCounterArrayW(c, fmt, &mut bytes, &mut count, items) };
        if r != 0 {
            return Vec::new();
        }
        let items = unsafe { std::slice::from_raw_parts(items, count as usize) };
        items
            .iter()
            .filter(|it| it.FmtValue.CStatus <= 1) // PDH_CSTATUS_VALID_DATA or NEW_DATA
            .map(|it| (from_wide_ptr(it.szName), unsafe { it.FmtValue.Anonymous.doubleValue }))
            .collect()
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.q) };
    }
}
