// Ported from miri/tests/fail/tree_borrows/reservedim_spurious_write.rs.
//@compile-flags: -Zmiri-tree-borrows -Zmiri-deterministic-concurrency

use std::cell::Cell;
use std::sync::{Arc, Barrier};
use std::thread;

#[derive(Copy, Clone)]
struct SendPtr(*mut u8);

unsafe impl Send for SendPtr {}

type IdxBarrier = (usize, Arc<Barrier>);

macro_rules! synchronized {
    ($thread:expr, $msg:expr) => {{
        let (thread_id, barrier) = &$thread;
        eprintln!("Thread {} executing: {}", thread_id, $msg);
        barrier.wait();
    }};
}

fn main() {
    let mut data = 0u8;
    let ptr = SendPtr(std::ptr::addr_of_mut!(data));
    let barrier = Arc::new(Barrier::new(2));
    let bx = Arc::clone(&barrier);
    let by = Arc::clone(&barrier);

    let thread_1 = thread::spawn(move || {
        let b = (1, bx);
        synchronized!(b, "start");
        let ptr = ptr;
        synchronized!(b, "retag x (&mut, protect)");
        fn inner(x: &mut u8, b: IdxBarrier) {
            *x = 42;
            synchronized!(b, "[lazy] retag y (&mut, protect, IM)");
            if cfg!(with) {
                synchronized!(b, "spurious write x (executed)");
                *x = 64;
            } else {
                synchronized!(b, "spurious write x (skipped)");
            }
            synchronized!(b, "ret y");
            synchronized!(b, "ret x");
        }
        inner(unsafe { &mut *ptr.0 }, b.clone());
        synchronized!(b, "write y");
        synchronized!(b, "end");
    });

    let thread_2 = thread::spawn(move || {
        let b = (2, by);
        synchronized!(b, "start");
        let ptr = ptr;
        synchronized!(b, "retag x (&mut, protect)");
        synchronized!(b, "[lazy] retag y (&mut, protect, IM)");
        fn inner(y: &mut Cell<()>, b: IdxBarrier) -> *mut u8 {
            synchronized!(b, "spurious write x");
            synchronized!(b, "ret y");
            y as *mut Cell<()> as *mut u8
        }
        let y_zst = unsafe { &mut *(ptr.0 as *mut Cell<()>) };
        let y = inner(y_zst, b.clone());
        synchronized!(b, "ret x");
        synchronized!(b, "write y");
        unsafe {
            *y = 13;
        }
        synchronized!(b, "end");
    });

    thread_1.join().unwrap();
    thread_2.join().unwrap();
}
