use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

pub(crate) struct Mutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

impl<T> Mutex<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub(crate) fn lock(&self) -> LockResult<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        LockResult(MutexGuard { lock: self })
    }
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

pub(crate) struct LockResult<'a, T>(MutexGuard<'a, T>);

impl<'a, T> LockResult<'a, T> {
    pub(crate) fn unwrap(self) -> MutexGuard<'a, T> {
        self.0
    }
}

pub(crate) struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

const ONCE_UNINIT: u8 = 0;
const ONCE_INITING: u8 = 1;
const ONCE_INIT: u8 = 2;

pub(crate) struct OnceLock<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> OnceLock<T> {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(ONCE_UNINIT),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub(crate) fn get_or_init(&self, init: impl FnOnce() -> T) -> &T {
        if let Some(value) = self.get() {
            return value;
        }

        loop {
            match self.state.compare_exchange(
                ONCE_UNINIT,
                ONCE_INITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    struct ResetOnDrop<'a> {
                        state: &'a AtomicU8,
                        armed: bool,
                    }

                    impl Drop for ResetOnDrop<'_> {
                        fn drop(&mut self) {
                            if self.armed {
                                self.state.store(ONCE_UNINIT, Ordering::Release);
                            }
                        }
                    }

                    let mut reset = ResetOnDrop {
                        state: &self.state,
                        armed: true,
                    };
                    unsafe {
                        (*self.value.get()).write(init());
                    }
                    reset.armed = false;
                    self.state.store(ONCE_INIT, Ordering::Release);
                    return unsafe { self.assume_init_ref() };
                }
                Err(ONCE_INIT) => return unsafe { self.assume_init_ref() },
                Err(ONCE_INITING) => {
                    while self.state.load(Ordering::Acquire) == ONCE_INITING {
                        spin_loop();
                    }
                }
                Err(_) => {}
            }
        }
    }

    pub(crate) fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == ONCE_INIT {
            Some(unsafe { self.assume_init_ref() })
        } else {
            None
        }
    }

    unsafe fn assume_init_ref(&self) -> &T {
        &*(*self.value.get()).as_ptr()
    }
}

unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}
unsafe impl<T: Send> Send for OnceLock<T> {}
