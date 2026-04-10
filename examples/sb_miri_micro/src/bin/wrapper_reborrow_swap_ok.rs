use std::hint::black_box;
use std::ops::{Deref, DerefMut};

struct Guard<T>(T);

impl<T> Deref for Guard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Clone, Copy)]
struct Inner {
    a: usize,
    b: usize,
    c: usize,
    d: usize,
}

#[inline(never)]
fn read_first(wrapper: &mut Guard<Inner>) -> usize {
    let inner = Guard::deref_mut(wrapper);
    black_box(inner.a)
}

#[inline(never)]
fn swap_inner(wrapper: &mut Guard<Inner>) {
    let inner = Guard::deref_mut(wrapper);
    let mut new_inner = Inner {
        a: inner.a ^ 1,
        b: inner.b.wrapping_add(1),
        c: inner.c,
        d: inner.d,
    };
    std::mem::swap(inner, &mut new_inner);
    black_box(new_inner.a ^ new_inner.b ^ new_inner.c ^ new_inner.d);
}

fn main() {
    let mut guard = Guard(Inner {
        a: 1,
        b: 2,
        c: 3,
        d: 4,
    });

    let first = {
        let wrapper = &mut guard;
        read_first(wrapper)
    };

    if first != 0 {
        let wrapper = &mut guard;
        swap_inner(wrapper);
    }

    let wrapper = &mut guard;
    let inner = Guard::deref_mut(wrapper);
    black_box(inner.a ^ inner.b ^ inner.c ^ inner.d);
}
