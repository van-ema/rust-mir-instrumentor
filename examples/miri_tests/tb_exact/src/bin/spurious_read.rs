// Ported from miri/tests/fail/tree_borrows/spurious_read.rs.
//@compile-flags: -Zmiri-deterministic-concurrency
//@compile-flags: -Zmiri-tree-borrows

use std::sync::{Arc, Barrier};
use std::thread;

#[derive(Copy, Clone)]
struct SendPtr(*mut u8);

unsafe impl Send for SendPtr {}

macro_rules! synchronized {
    ($thread:expr, $msg:expr) => {{
        let (thread_id, barrier) = &$thread;
        eprintln!("Thread {} executing: {}", thread_id, $msg);
        barrier.wait();
    }};
}

fn main() {
    retagx_retagy_retx_writey_rety();
}

fn retagx_retagy_retx_writey_rety() {
    let mut data = 0u8;
    let ptr = SendPtr(std::ptr::addr_of_mut!(data));
    let barrier = Arc::new(Barrier::new(2));
    let bx = Arc::clone(&barrier);
    let by = Arc::clone(&barrier);

    let thread_x = thread::spawn(move || {
        let b = (1, bx);
        synchronized!(b, "start");
        let ptr = ptr;
        synchronized!(b, "retag x (&mut, protect)");
        fn as_mut(x: &mut u8, b: (usize, Arc<Barrier>)) -> *mut u8 {
            synchronized!(b, "retag y (&mut, protect)");
            synchronized!(b, "location where spurious read of x would happen in the target");
            synchronized!(b, "ret x");
            synchronized!(b, "write y");
            x as *mut u8
        }
        let _x = as_mut(unsafe { &mut *ptr.0 }, b.clone());
        synchronized!(b, "ret y");
        synchronized!(b, "end");
    });

    let thread_y = thread::spawn(move || {
        let b = (2, by);
        synchronized!(b, "start");
        let ptr = ptr;
        synchronized!(b, "retag x (&mut, protect)");
        synchronized!(b, "retag y (&mut, protect)");
        fn as_mut(y: &mut u8, b: (usize, Arc<Barrier>)) -> *mut u8 {
            synchronized!(b, "location where spurious read of x would happen in the target");
            synchronized!(b, "ret x");
            let y = y as *mut u8;
            synchronized!(b, "write y");
            unsafe {
                *y = 2;
            }
            synchronized!(b, "ret y");
            y
        }
        let _y = as_mut(unsafe { &mut *ptr.0 }, b.clone());
        synchronized!(b, "end");
    });

    thread_x.join().unwrap();
    thread_y.join().unwrap();
}
