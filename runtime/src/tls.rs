use core::ffi::{c_int, c_void};

use crate::compat::abort_process;
use crate::sync::OnceLock;

#[cfg(target_os = "macos")]
type PthreadKey = usize;
#[cfg(target_os = "linux")]
type PthreadKey = u32;

#[cfg(target_os = "linux")]
#[link(name = "pthread")]
unsafe extern "C" {}

unsafe extern "C" {
    fn pthread_key_create(
        key: *mut PthreadKey,
        dtor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn pthread_getspecific(key: PthreadKey) -> *mut c_void;
    fn pthread_setspecific(key: PthreadKey, value: *const c_void) -> c_int;
}

fn alloc_hook_key() -> PthreadKey {
    static KEY: OnceLock<PthreadKey> = OnceLock::new();
    *KEY.get_or_init(create_key)
}

fn runtime_depth_key() -> PthreadKey {
    static KEY: OnceLock<PthreadKey> = OnceLock::new();
    *KEY.get_or_init(create_key)
}

fn sb_suppress_key() -> PthreadKey {
    static KEY: OnceLock<PthreadKey> = OnceLock::new();
    *KEY.get_or_init(create_key)
}

fn relax_epoch_key() -> PthreadKey {
    static KEY: OnceLock<PthreadKey> = OnceLock::new();
    *KEY.get_or_init(create_key)
}

fn create_key() -> PthreadKey {
    let mut key = 0;
    let rc = unsafe { pthread_key_create(&mut key, None) };
    if rc != 0 {
        abort_process();
    }
    key
}

fn get_usize(key: PthreadKey) -> usize {
    unsafe { pthread_getspecific(key) as usize }
}

fn set_usize(key: PthreadKey, value: usize) {
    let rc = unsafe { pthread_setspecific(key, value as *const c_void) };
    if rc != 0 {
        abort_process();
    }
}

pub(crate) fn in_alloc_hook() -> bool {
    get_usize(alloc_hook_key()) != 0
}

pub(crate) fn set_in_alloc_hook(value: bool) {
    set_usize(alloc_hook_key(), usize::from(value));
}

pub(crate) fn runtime_depth() -> u32 {
    get_usize(runtime_depth_key()) as u32
}

pub(crate) fn set_runtime_depth(value: u32) {
    set_usize(runtime_depth_key(), value as usize);
}

pub(crate) fn sb_suppressed() -> bool {
    get_usize(sb_suppress_key()) != 0
}

pub(crate) fn set_sb_suppressed(value: bool) {
    set_usize(sb_suppress_key(), usize::from(value));
}

pub(crate) fn relax_epoch_depth() -> u32 {
    get_usize(relax_epoch_key()) as u32
}

pub(crate) fn set_relax_epoch_depth(value: u32) {
    set_usize(relax_epoch_key(), value as usize);
}
