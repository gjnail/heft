//! PawnIO, a signed open-source driver (pawnio.eu) that runs small, signed
//! modules for low-level hardware access. It's what LibreHardwareMonitor and
//! FanControl use. Heft never installs it: when the user has, and Heft runs
//! as administrator, Heft loads the modules it needs from the official
//! PawnIO.Modules release (embedded below, LGPL-2.1, see assets/pawnio).
//!
//! Each open handle holds one loaded module. The driver checks the module's
//! signature, and each module only allows the registers and ports its
//! hardware needs.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use crate::winsys::wide;

pub static AMD_FAMILY_17: &[u8] = include_bytes!("../../../assets/pawnio/AMDFamily17.bin");
pub static INTEL_MSR: &[u8] = include_bytes!("../../../assets/pawnio/IntelMSR.bin");
pub static LPC_IO: &[u8] = include_bytes!("../../../assets/pawnio/LpcIO.bin");

const DEVICE: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const DEVICE_TYPE: u32 = 41394;
const IOCTL_LOAD_BINARY: u32 = ctl_code(0x821);
const IOCTL_EXECUTE_FN: u32 = ctl_code(0x841);
const FN_NAME_LENGTH: usize = 32;

const fn ctl_code(function: u32) -> u32 {
    // METHOD_BUFFERED, FILE_ANY_ACCESS
    (DEVICE_TYPE << 16) | (function << 2)
}

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Why PawnIO can't be used.
#[derive(Clone, Debug, PartialEq)]
pub enum Unavailable {
    NotInstalled,
    NeedsAdmin,
    Failed(String),
}

/// A PawnIO handle with one module loaded.
pub struct Module {
    h: HANDLE,
}

// SAFETY: the handle is only used from the sampling thread that owns it.
unsafe impl Send for Module {}

impl Module {
    pub fn load(blob: &[u8]) -> Result<Module, Unavailable> {
        let path = wide(DEVICE);
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let e = unsafe { GetLastError() };
            return Err(match e {
                ERROR_ACCESS_DENIED => Unavailable::NeedsAdmin,
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND if installed() => {
                    Unavailable::Failed("PawnIO is installed but its driver isn't running. Restarting Windows usually fixes this.".into())
                }
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Unavailable::NotInstalled,
                e => Unavailable::Failed(format!("Could not open PawnIO: {}", crate::reg::win_error(e))),
            });
        }
        let m = Module { h };
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                m.h,
                IOCTL_LOAD_BINARY,
                blob.as_ptr() as *const _,
                blob.len() as u32,
                null_mut(),
                0,
                &mut ret,
                null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            return Err(Unavailable::Failed(format!("PawnIO refused a module: {}", crate::reg::win_error(e))));
        }
        Ok(m)
    }

    /// Run a module function. Returns how many output values it wrote, or
    /// the Windows error code.
    pub fn call(&self, name: &str, input: &[u64], out: &mut [u64]) -> Result<usize, u32> {
        debug_assert!(name.len() < FN_NAME_LENGTH);
        let mut buf = vec![0u8; FN_NAME_LENGTH + input.len() * 8];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        for (i, v) in input.iter().enumerate() {
            let at = FN_NAME_LENGTH + i * 8;
            buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
        }
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                IOCTL_EXECUTE_FN,
                buf.as_ptr() as *const _,
                buf.len() as u32,
                out.as_mut_ptr() as *mut _,
                (out.len() * 8) as u32,
                &mut ret,
                null_mut(),
            )
        };
        if ok == 0 { Err(unsafe { GetLastError() }) } else { Ok(ret as usize / 8) }
    }

    /// A function that takes some values and returns one.
    pub fn get(&self, name: &str, input: &[u64]) -> Option<u64> {
        let mut out = [0u64; 1];
        match self.call(name, input, &mut out) {
            Ok(1) => Some(out[0]),
            _ => None,
        }
    }

    /// A function that returns nothing.
    pub fn run(&self, name: &str, input: &[u64]) -> bool {
        self.call(name, input, &mut []).is_ok()
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.h) };
    }
}

/// Whether the PawnIO service is registered, even if its device isn't there.
fn installed() -> bool {
    crate::reg::Key::open(crate::reg::Hive::LocalMachine, r"SYSTEM\CurrentControlSet\Services\PawnIO").is_some()
}

/// A named mutex that hardware monitoring tools share so they don't talk to
/// the same chip at the same time.
pub struct BusMutex(HANDLE);

unsafe impl Send for BusMutex {}

/// Shared with HWiNFO, AIDA64, LibreHardwareMonitor and vendor tools.
pub const ISA_BUS: &str = r"Global\Access_ISABUS.HTP.Method";
pub const PCI_BUS: &str = r"Global\Access_PCI";

impl BusMutex {
    pub fn new(name: &str) -> Option<BusMutex> {
        let w = wide(name);
        // Opens the existing one when another tool created it first.
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, w.as_ptr()) };
        (!h.is_null()).then_some(BusMutex(h))
    }

    /// Hold the bus while `f` runs, or skip `f` if another tool keeps it
    /// busy for longer than `timeout_ms`.
    pub fn with<R>(&self, timeout_ms: u32, f: impl FnOnce() -> R) -> Option<R> {
        let r = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        if r != WAIT_OBJECT_0 && r != WAIT_ABANDONED {
            return None;
        }
        let out = f();
        unsafe { ReleaseMutex(self.0) };
        Some(out)
    }
}

impl Drop for BusMutex {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
