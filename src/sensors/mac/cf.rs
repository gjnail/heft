//! Just enough CoreFoundation and IOKit to read the I/O Registry and talk to
//! drivers, with every object released when its owner is dropped.

use std::ffi::{c_char, c_void, CString};
use std::marker::PhantomData;

pub type CFTypeRef = *const c_void;
type CFIndex = isize;
type CFTypeID = usize;

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT64: CFIndex = 4;
const NUMBER_FLOAT64: CFIndex = 6;

/// Opaque CoreFoundation callback tables, only ever used by address.
#[repr(C)]
pub struct CallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFArrayGetTypeID() -> CFTypeID;
    fn CFDataGetTypeID() -> CFTypeID;
    fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: CFIndex, encoding: u32, external: u8) -> CFTypeRef;
    fn CFStringGetLength(s: CFTypeRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(len: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFNumberCreate(alloc: CFTypeRef, kind: CFIndex, value: *const c_void) -> CFTypeRef;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: CFIndex,
        key_cb: *const c_void,
        value_cb: *const c_void,
    ) -> CFTypeRef;
    fn CFArrayGetCount(a: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: CFIndex) -> CFTypeRef;
    fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    fn CFUUIDCreateFromUUIDBytes(alloc: CFTypeRef, bytes: UuidBytes) -> CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: CallBacks;
    static kCFTypeDictionaryValueCallBacks: CallBacks;
}

/// A UUID's bytes (CFUUIDBytes), passed by value.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UuidBytes(pub [u8; 16]);

/// A CoreFoundation object this code owns.
pub struct Cf(CFTypeRef);

impl Cf {
    /// Takes over a reference from a Create or Copy function; `None` for null.
    ///
    /// # Safety
    /// `r` must be null or an owned reference to a CoreFoundation object.
    pub unsafe fn owned(r: CFTypeRef) -> Option<Cf> {
        (!r.is_null()).then_some(Cf(r))
    }

    pub fn string(s: &str) -> Cf {
        Cf(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, UTF8, 0) })
    }

    pub fn uuid(bytes: UuidBytes) -> Cf {
        Cf(unsafe { CFUUIDCreateFromUUIDBytes(std::ptr::null(), bytes) })
    }

    pub fn number(v: i64) -> Cf {
        Cf(unsafe { CFNumberCreate(std::ptr::null(), NUMBER_SINT64, &v as *const i64 as *const c_void) })
    }

    /// A dictionary of the given string keys and objects.
    pub fn dictionary(pairs: &[(&str, &Cf)]) -> Cf {
        let keys: Vec<Cf> = pairs.iter().map(|(k, _)| Cf::string(k)).collect();
        let key_refs: Vec<CFTypeRef> = keys.iter().map(|k| k.0).collect();
        let values: Vec<CFTypeRef> = pairs.iter().map(|(_, v)| v.0).collect();
        Cf(unsafe {
            CFDictionaryCreate(
                std::ptr::null(),
                key_refs.as_ptr(),
                values.as_ptr(),
                pairs.len() as CFIndex,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            )
        })
    }

    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }

    pub fn obj(&self) -> Obj<'_> {
        Obj { ptr: self.0, _owner: PhantomData }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// A CoreFoundation object borrowed from something that outlives `'a`.
#[derive(Clone, Copy)]
pub struct Obj<'a> {
    ptr: CFTypeRef,
    _owner: PhantomData<&'a ()>,
}

impl<'a> Obj<'a> {
    /// # Safety
    /// `ptr` must be null or a CoreFoundation object that stays alive for `'a`.
    pub unsafe fn borrowed(ptr: CFTypeRef) -> Option<Obj<'a>> {
        (!ptr.is_null()).then_some(Obj { ptr, _owner: PhantomData })
    }

    pub fn as_ptr(self) -> CFTypeRef {
        self.ptr
    }

    fn is(self, id: CFTypeID) -> bool {
        unsafe { CFGetTypeID(self.ptr) == id }
    }

    /// The value for a string key, if this is a dictionary.
    pub fn get(self, key: &str) -> Option<Obj<'a>> {
        if !self.is(unsafe { CFDictionaryGetTypeID() }) {
            return None;
        }
        let k = Cf::string(key);
        unsafe { Obj::borrowed(CFDictionaryGetValue(self.ptr, k.0)) }
    }

    pub fn string(self) -> Option<String> {
        if !self.is(unsafe { CFStringGetTypeID() }) {
            return None;
        }
        let len = unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(self.ptr), UTF8) } + 1;
        let mut buf = vec![0u8; len.max(1) as usize];
        if unsafe { CFStringGetCString(self.ptr, buf.as_mut_ptr() as *mut c_char, buf.len() as CFIndex, UTF8) } == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        String::from_utf8(buf).ok()
    }

    /// A number or boolean as an integer.
    pub fn i64(self) -> Option<i64> {
        if self.is(unsafe { CFBooleanGetTypeID() }) {
            return Some(unsafe { CFBooleanGetValue(self.ptr) } as i64);
        }
        if !self.is(unsafe { CFNumberGetTypeID() }) {
            return None;
        }
        let mut v = 0i64;
        (unsafe { CFNumberGetValue(self.ptr, NUMBER_SINT64, &mut v as *mut i64 as *mut c_void) } != 0).then_some(v)
    }

    pub fn f64(self) -> Option<f64> {
        if !self.is(unsafe { CFNumberGetTypeID() }) {
            return self.i64().map(|v| v as f64);
        }
        let mut v = 0f64;
        (unsafe { CFNumberGetValue(self.ptr, NUMBER_FLOAT64, &mut v as *mut f64 as *mut c_void) } != 0).then_some(v)
    }

    pub fn bool(self) -> Option<bool> {
        self.i64().map(|v| v != 0)
    }

    /// The elements, if this is an array.
    pub fn items(self) -> Vec<Obj<'a>> {
        if !self.is(unsafe { CFArrayGetTypeID() }) {
            return Vec::new();
        }
        let n = unsafe { CFArrayGetCount(self.ptr) };
        (0..n).filter_map(|i| unsafe { Obj::borrowed(CFArrayGetValueAtIndex(self.ptr, i)) }).collect()
    }

    /// The bytes, if this is a data object.
    pub fn bytes(self) -> Option<&'a [u8]> {
        if !self.is(unsafe { CFDataGetTypeID() }) {
            return None;
        }
        let len = unsafe { CFDataGetLength(self.ptr) } as usize;
        let p = unsafe { CFDataGetBytePtr(self.ptr) };
        if p.is_null() || len == 0 {
            return Some(&[]);
        }
        Some(unsafe { std::slice::from_raw_parts(p, len) })
    }
}

// ----------------------------------------------------------------------
// IOKit

const MAIN_PORT: u32 = 0;
const SERVICE_PLANE: &[u8] = b"IOService\0";

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFTypeRef;
    fn IOBSDNameMatching(main_port: u32, options: u32, bsd_name: *const c_char) -> CFTypeRef;
    fn IOServiceGetMatchingServices(main_port: u32, matching: CFTypeRef, iter: *mut u32) -> i32;
    fn IOServiceGetMatchingService(main_port: u32, matching: CFTypeRef) -> u32;
    fn IOIteratorNext(iter: u32) -> u32;
    fn IOObjectRelease(obj: u32) -> i32;
    fn IOObjectConformsTo(obj: u32, class: *const c_char) -> u8;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: CFTypeRef, alloc: CFTypeRef, options: u32) -> CFTypeRef;
    fn IORegistryEntryGetParentEntry(entry: u32, plane: *const c_char, parent: *mut u32) -> i32;
    fn IORegistryEntryGetChildIterator(entry: u32, plane: *const c_char, iter: *mut u32) -> i32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
    fn IORegistryEntryFromPath(main_port: u32, path: *const c_char) -> u32;
    pub fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
    pub fn IOServiceClose(connect: u32) -> i32;
    pub fn IOConnectCallStructMethod(
        connect: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
    pub fn IOCreatePlugInInterfaceForService(
        service: u32,
        plugin_type: CFTypeRef,
        interface_type: CFTypeRef,
        the_interface: *mut *mut *mut c_void,
        score: *mut i32,
    ) -> i32;
}

/// An I/O Registry entry (or iterator) this code holds a reference to.
pub struct IoObject(u32);

impl Drop for IoObject {
    fn drop(&mut self) {
        unsafe { IOObjectRelease(self.0) };
    }
}

fn drain(iter: u32) -> Vec<IoObject> {
    let mut out = Vec::new();
    loop {
        let o = unsafe { IOIteratorNext(iter) };
        if o == 0 {
            break;
        }
        out.push(IoObject(o));
    }
    unsafe { IOObjectRelease(iter) };
    out
}

impl IoObject {
    /// Every service of this class (or a subclass).
    pub fn matching(class: &str) -> Vec<IoObject> {
        let Ok(c) = CString::new(class) else { return Vec::new() };
        let mut iter = 0u32;
        // The matching dictionary is consumed by the call.
        if unsafe { IOServiceGetMatchingServices(MAIN_PORT, IOServiceMatching(c.as_ptr()), &mut iter) } != 0 {
            return Vec::new();
        }
        drain(iter)
    }

    pub fn first(class: &str) -> Option<IoObject> {
        let c = CString::new(class).ok()?;
        let o = unsafe { IOServiceGetMatchingService(MAIN_PORT, IOServiceMatching(c.as_ptr())) };
        (o != 0).then_some(IoObject(o))
    }

    /// The media object for a BSD disk name such as "disk3s1".
    pub fn bsd(name: &str) -> Option<IoObject> {
        let c = CString::new(name).ok()?;
        let o = unsafe { IOServiceGetMatchingService(MAIN_PORT, IOBSDNameMatching(MAIN_PORT, 0, c.as_ptr())) };
        (o != 0).then_some(IoObject(o))
    }

    /// An entry by path, such as "IODeviceTree:/cpus".
    pub fn at_path(path: &str) -> Option<IoObject> {
        let c = CString::new(path).ok()?;
        let o = unsafe { IORegistryEntryFromPath(MAIN_PORT, c.as_ptr()) };
        (o != 0).then_some(IoObject(o))
    }

    pub fn raw(&self) -> u32 {
        self.0
    }

    pub fn property(&self, key: &str) -> Option<Cf> {
        let k = Cf::string(key);
        unsafe { Cf::owned(IORegistryEntryCreateCFProperty(self.0, k.as_ptr(), std::ptr::null(), 0)) }
    }

    pub fn parent(&self) -> Option<IoObject> {
        let mut p = 0u32;
        let r = unsafe { IORegistryEntryGetParentEntry(self.0, SERVICE_PLANE.as_ptr() as *const c_char, &mut p) };
        (r == 0 && p != 0).then_some(IoObject(p))
    }

    /// Children in the service plane (or the device tree, for device tree entries).
    pub fn children(&self, plane: &str) -> Vec<IoObject> {
        let Ok(c) = CString::new(plane) else { return Vec::new() };
        let mut iter = 0u32;
        if unsafe { IORegistryEntryGetChildIterator(self.0, c.as_ptr(), &mut iter) } != 0 {
            return Vec::new();
        }
        drain(iter)
    }

    pub fn conforms_to(&self, class: &str) -> bool {
        CString::new(class).is_ok_and(|c| unsafe { IOObjectConformsTo(self.0, c.as_ptr()) } != 0)
    }

    pub fn id(&self) -> u64 {
        let mut id = 0u64;
        unsafe { IORegistryEntryGetRegistryEntryID(self.0, &mut id) };
        id
    }
}

/// A string property that is either a CFString or NUL-terminated data (the
/// device tree stores strings as data).
pub fn text(o: Obj) -> Option<String> {
    if let Some(s) = o.string() {
        return Some(s);
    }
    let b = o.bytes()?;
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    Some(String::from_utf8_lossy(&b[..end]).trim().to_string()).filter(|s| !s.is_empty())
}

// ----------------------------------------------------------------------
// Private frameworks, looked up at run time

/// A system library opened with dlopen. It stays loaded for the life of
/// the process, so functions taken from it never dangle.
pub struct Library(*mut c_void);

impl Library {
    pub fn open(path: &str) -> Option<Library> {
        let c = CString::new(path).ok()?;
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY) };
        (!h.is_null()).then_some(Library(h))
    }

    /// A function by name, as the function pointer type `F`. A macOS release
    /// without it just loses the readings that need it.
    ///
    /// # Safety
    /// `F` must be an `extern "C" fn` type matching the symbol's real signature.
    pub unsafe fn get<F: Copy>(&self, name: &str) -> Option<F> {
        assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
        let c = CString::new(name).ok()?;
        let p = unsafe { libc::dlsym(self.0, c.as_ptr()) };
        (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let s = Cf::string("Héft");
        assert_eq!(s.obj().string().as_deref(), Some("Héft"));
        let n = Cf::number(-42);
        assert_eq!(n.obj().i64(), Some(-42));
        assert_eq!(n.obj().f64(), Some(-42.0));
        assert_eq!(n.obj().string(), None);
        let d = Cf::dictionary(&[("a", &n), ("b", &s)]);
        assert_eq!(d.obj().get("a").and_then(|v| v.i64()), Some(-42));
        assert_eq!(d.obj().get("b").and_then(|v| v.string()).as_deref(), Some("Héft"));
        assert!(d.obj().get("c").is_none());
        assert!(n.obj().get("a").is_none());
    }
}
