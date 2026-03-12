use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::cmp;
use core::ffi::{c_char, c_int, c_void, CStr};
use core::ptr;

pub(crate) use alloc::collections::BTreeMap;
pub(crate) type HashMap<K, V> = BTreeMap<K, V>;
pub(crate) use alloc::string::String as RzString;
pub(crate) use alloc::vec::Vec as RzVec;

#[cfg(target_os = "linux")]
#[link(name = "pthread")]
unsafe extern "C" {}

unsafe extern "C" {
    fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
    fn abort() -> !;
    fn getenv(name: *const c_char) -> *const c_char;
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(nmemb: usize, size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> c_int;
}

pub(crate) fn write_stderr(bytes: &[u8]) {
    unsafe {
        let _ = write(2, bytes.as_ptr(), bytes.len());
    }
}

pub(crate) fn abort_process() -> ! {
    unsafe { abort() }
}

pub(crate) fn env_var_owned(name: &str) -> Option<String> {
    let mut key = Vec::with_capacity(name.len() + 1);
    key.extend_from_slice(name.as_bytes());
    key.push(0);
    let value = unsafe { getenv(key.as_ptr().cast()) };
    if value.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(value) };
    Some(String::from_utf8_lossy(value.to_bytes()).into_owned())
}

pub(crate) fn env_var_lowercase(name: &str) -> Option<String> {
    env_var_owned(name).map(|value| value.to_ascii_lowercase())
}

pub(crate) fn env_flag(name: &str, default: bool) -> bool {
    match env_var_owned(name) {
        None => default,
        Some(value) => !matches!(value.as_str(), "0") && !value.eq_ignore_ascii_case("false"),
    }
}

pub(crate) fn capture_backtrace() -> Option<String> {
    #[cfg(feature = "std")]
    {
        Some(format!("{:?}", std::backtrace::Backtrace::force_capture()))
    }

    #[cfg(not(feature = "std"))]
    {
        None
    }
}

const MIN_ALIGN: usize = if cfg!(target_pointer_width = "64") { 16 } else { 8 };

pub(crate) struct PlatformAlloc;

unsafe impl GlobalAlloc for PlatformAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        alloc_with_alignment(layout.align(), layout.size(), false)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        alloc_with_alignment(layout.align(), layout.size(), true)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if !ptr.is_null() {
            free(ptr.cast());
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(Layout::from_size_align_unchecked(new_size.max(1), layout.align()));
        }
        if new_size == 0 {
            self.dealloc(ptr, layout);
            return ptr::null_mut();
        }
        if layout.align() <= MIN_ALIGN {
            return realloc(ptr.cast(), new_size.max(1)).cast();
        }

        let new_ptr = alloc_with_alignment(layout.align(), new_size, false);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }
        let copy_len = cmp::min(layout.size(), new_size);
        if copy_len != 0 {
            let _ = memcpy(new_ptr.cast(), ptr.cast(), copy_len);
        }
        free(ptr.cast());
        new_ptr
    }
}

unsafe fn alloc_with_alignment(align: usize, size: usize, zeroed: bool) -> *mut u8 {
    let size = size.max(1);
    if align <= MIN_ALIGN {
        let raw = if zeroed {
            calloc(1, size)
        } else {
            malloc(size)
        };
        return raw.cast();
    }

    let mut out = ptr::null_mut();
    if posix_memalign(&mut out, align, size) != 0 || out.is_null() {
        return ptr::null_mut();
    }
    if zeroed {
        ptr::write_bytes(out.cast::<u8>(), 0, size);
    }
    out.cast()
}
