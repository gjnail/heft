//! The notification-area icon (Windows). With it, Heft can keep watching free
//! space after its window is closed, show low-space warnings as
//! notifications, and start with Windows.
//!
//! The icon lives on its own thread with a hidden message window, so it keeps
//! working while the main window is hidden and not drawing.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

use crate::platform::wide;
use crate::reg::{self, Hive};

const WM_TRAY: u32 = wm::WM_APP + 1;
const WM_BALLOON: u32 = wm::WM_APP + 2;
const OPEN: usize = 1;
const QUIT: usize = 2;

/// The hidden message window, or 0 when the icon isn't showing.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The icon brought the main window back; the app syncs its own state.
static SHOWN: AtomicBool = AtomicBool::new(false);
/// "Quit Heft" was chosen, so closing the window should really quit.
static QUITTING: AtomicBool = AtomicBool::new(false);
static BALLOON: Mutex<Option<(String, String)>> = Mutex::new(None);

pub fn running() -> bool {
    WINDOW.load(Ordering::Acquire) != 0
}

/// Show the icon (does nothing if it's already there).
pub fn start() {
    if running() {
        return;
    }
    let _ = std::thread::Builder::new().name("tray".into()).spawn(|| unsafe { run() });
}

/// Remove the icon.
pub fn stop() {
    let h = WINDOW.load(Ordering::Acquire);
    if h != 0 {
        unsafe { wm::PostMessageW(h as HWND, wm::WM_CLOSE, 0, 0) };
    }
}

pub fn take_shown() -> bool {
    SHOWN.swap(false, Ordering::AcqRel)
}

pub fn quitting() -> bool {
    QUITTING.load(Ordering::Acquire)
}

/// A notification from the icon if it's showing; otherwise flash Heft's
/// taskbar button.
pub fn notify(title: &str, body: &str) {
    let h = WINDOW.load(Ordering::Acquire);
    if h != 0 {
        *BALLOON.lock().unwrap() = Some((title.to_string(), body.to_string()));
        unsafe { wm::PostMessageW(h as HWND, WM_BALLOON, 0, 0) };
    } else if let Some(main) = main_window() {
        let f = wm::FLASHWINFO {
            cbSize: size_of::<wm::FLASHWINFO>() as u32,
            hwnd: main,
            dwFlags: wm::FLASHW_TRAY | wm::FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        unsafe { wm::FlashWindowEx(&f) };
    }
}

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Heft";

/// Heft is set to start, in the notification area, when you sign in.
pub fn starts_with_windows() -> bool {
    reg::Key::open(Hive::CurrentUser, RUN).and_then(|k| k.get_string(RUN_VALUE)).is_some_and(|v| v.contains("--tray"))
}

pub fn set_start_with_windows(on: bool) -> Result<(), String> {
    let key = reg::Key::create(Hive::CurrentUser, RUN)?;
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmd = format!("\"{}\" --tray", exe.display());
        let bytes: Vec<u8> = wide(&cmd).iter().flat_map(|c| c.to_le_bytes()).collect();
        key.set_raw(RUN_VALUE, reg::REG_SZ, &bytes)
    } else if key.get(RUN_VALUE).is_some() {
        key.delete_value(RUN_VALUE)
    } else {
        Ok(())
    }
}

/// Heft's main window: the visible or hidden top-level window of this
/// process titled "Heft".
fn main_window() -> Option<HWND> {
    unsafe extern "system" fn each(h: HWND, found: LPARAM) -> i32 {
        let mut pid = 0u32;
        unsafe { wm::GetWindowThreadProcessId(h, &mut pid) };
        if pid != std::process::id() || h as isize == WINDOW.load(Ordering::Acquire) {
            return 1;
        }
        let mut title = [0u16; 16];
        let n = unsafe { wm::GetWindowTextW(h, title.as_mut_ptr(), title.len() as i32) };
        if String::from_utf16_lossy(&title[..n.max(0) as usize]) == "Heft" {
            unsafe { *(found as *mut HWND) = h };
            return 0;
        }
        1
    }
    let mut found: HWND = std::ptr::null_mut();
    unsafe { wm::EnumWindows(Some(each), &mut found as *mut HWND as LPARAM) };
    (!found.is_null()).then_some(found)
}

fn show_main() {
    if let Some(h) = main_window() {
        unsafe {
            wm::ShowWindow(h, if wm::IsIconic(h) != 0 { wm::SW_RESTORE } else { wm::SW_SHOW });
            wm::SetForegroundWindow(h);
        }
        SHOWN.store(true, Ordering::Release);
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

fn copy_into(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

unsafe fn add_icon(hwnd: HWND) {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    unsafe {
        let (cx, cy) = (wm::GetSystemMetrics(wm::SM_CXSMICON), wm::GetSystemMetrics(wm::SM_CYSMICON));
        // The app icon build.rs embeds as resource 1; the stock one if it's missing.
        let own = wm::LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, wm::IMAGE_ICON, cx, cy, wm::LR_DEFAULTCOLOR);
        nid.hIcon = if own.is_null() { wm::LoadIconW(std::ptr::null_mut(), wm::IDI_APPLICATION) } else { own };
    }
    copy_into(&mut nid.szTip, "Heft: watching free space");
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
}

unsafe fn run() {
    let class = wide("HeftTray");
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = wm::WNDCLASSEXW {
        cbSize: size_of::<wm::WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { wm::RegisterClassExW(&wc) };
    let hwnd = unsafe {
        wm::CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            wm::HWND_MESSAGE,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return;
    }
    if WINDOW.compare_exchange(0, hwnd as isize, Ordering::AcqRel, Ordering::Acquire).is_err() {
        unsafe { wm::DestroyWindow(hwnd) };
        return; // another icon won the race
    }
    unsafe { add_icon(hwnd) };
    let mut msg: wm::MSG = unsafe { std::mem::zeroed() };
    while unsafe { wm::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            wm::TranslateMessage(&msg);
            wm::DispatchMessageW(&msg);
        }
    }
    WINDOW.store(0, Ordering::Release);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let taskbar_created = *TASKBAR_CREATED.get_or_init(|| unsafe { wm::RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) });
    match msg {
        WM_TRAY => {
            match lparam as u32 {
                wm::WM_LBUTTONUP | NIN_BALLOONUSERCLICK => show_main(),
                wm::WM_RBUTTONUP => unsafe { menu(hwnd) },
                _ => {}
            }
            0
        }
        WM_BALLOON => {
            if let Some((title, body)) = BALLOON.lock().unwrap().take() {
                let mut nid = icon_data(hwnd);
                nid.uFlags = NIF_INFO;
                nid.dwInfoFlags = NIIF_WARNING;
                copy_into(&mut nid.szInfoTitle, &title);
                copy_into(&mut nid.szInfo, &body);
                unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
            }
            0
        }
        wm::WM_CLOSE => {
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
                wm::DestroyWindow(hwnd);
            }
            0
        }
        wm::WM_DESTROY => {
            unsafe { wm::PostQuitMessage(0) };
            0
        }
        // Explorer restarted and forgot the icon.
        m if m == taskbar_created => {
            unsafe { add_icon(hwnd) };
            0
        }
        _ => unsafe { wm::DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn menu(hwnd: HWND) {
    unsafe {
        let m = wm::CreatePopupMenu();
        wm::AppendMenuW(m, wm::MF_STRING, OPEN, wide("Open Heft").as_ptr());
        wm::AppendMenuW(m, wm::MF_STRING, QUIT, wide("Quit Heft").as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        wm::GetCursorPos(&mut pt);
        // Without this the menu doesn't close when you click elsewhere.
        wm::SetForegroundWindow(hwnd);
        let cmd = wm::TrackPopupMenu(m, wm::TPM_RETURNCMD | wm::TPM_NONOTIFY | wm::TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        wm::DestroyMenu(m);
        match cmd as usize {
            OPEN => show_main(),
            QUIT => {
                QUITTING.store(true, Ordering::Release);
                show_main();
                if let Some(h) = main_window() {
                    wm::PostMessageW(h, wm::WM_CLOSE, 0, 0);
                }
            }
            _ => {}
        }
    }
}

/// Debug builds: click the icon, close the window, click again, close again
/// and quit, logging whether the window is visible after each step to
/// heft-tray-selftest.txt in the temp folder.
#[cfg(debug_assertions)]
pub fn self_test() {
    std::thread::spawn(|| {
        let pause = || std::thread::sleep(std::time::Duration::from_secs(2));
        let mut log = String::new();
        let mut note = |step: &str| {
            let visible = main_window().is_some_and(|h| unsafe { wm::IsWindowVisible(h) } != 0);
            log.push_str(&format!("{step}: icon={} window visible={visible}\n", running()));
            let _ = std::fs::write(std::env::temp_dir().join("heft-tray-selftest.txt"), &log);
        };
        let click = || unsafe { wm::PostMessageW(WINDOW.load(Ordering::Acquire) as HWND, WM_TRAY, 0, wm::WM_LBUTTONUP as LPARAM) };
        let close = || {
            if let Some(h) = main_window() {
                unsafe { wm::PostMessageW(h, wm::WM_CLOSE, 0, 0) };
            }
        };
        std::thread::sleep(std::time::Duration::from_secs(3));
        note("started with --tray");
        click();
        pause();
        note("clicked the icon");
        close();
        pause();
        note("closed the window");
        click();
        pause();
        note("clicked the icon again");
        close();
        pause();
        note("closed it again");
        note("quitting");
        QUITTING.store(true, Ordering::Release);
        show_main();
        close();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes and removes the real Run entry (skipped if one is already set).
    #[test]
    #[ignore]
    fn start_with_windows_round_trip() {
        if reg::Key::open(Hive::CurrentUser, RUN).and_then(|k| k.get(RUN_VALUE)).is_some() {
            return;
        }
        set_start_with_windows(true).unwrap();
        assert!(starts_with_windows());
        let cmd = reg::Key::open(Hive::CurrentUser, RUN).and_then(|k| k.get_string(RUN_VALUE)).unwrap();
        assert!(cmd.starts_with('"') && cmd.ends_with("\" --tray"), "{cmd}");
        set_start_with_windows(false).unwrap();
        assert!(!starts_with_windows());
        assert!(reg::Key::open(Hive::CurrentUser, RUN).and_then(|k| k.get(RUN_VALUE)).is_none());
    }

    /// Puts a real icon in the notification area for a moment.
    #[test]
    #[ignore]
    fn icon_comes_and_goes() {
        start();
        for _ in 0..50 {
            if running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(running(), "icon thread didn't start: {}", std::io::Error::last_os_error());
        stop();
        for _ in 0..50 {
            if !running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!running());
    }
}
