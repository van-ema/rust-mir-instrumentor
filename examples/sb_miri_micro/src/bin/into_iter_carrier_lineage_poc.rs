use std::hint::black_box;

struct Carrier {
    buf: Vec<u8>,
}

struct CarrierIntoIter {
    inner: std::vec::IntoIter<u8>,
}

impl IntoIterator for Carrier {
    type Item = u8;
    type IntoIter = CarrierIntoIter;

    #[inline(never)]
    fn into_iter(self) -> Self::IntoIter {
        CarrierIntoIter {
            inner: self.buf.into_iter(),
        }
    }
}

impl Iterator for CarrierIntoIter {
    type Item = u8;

    #[inline(never)]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    #[inline(never)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[inline(never)]
fn consume<I: IntoIterator<Item = u8>>(src: I) -> usize {
    let iter = src.into_iter();
    let hint = (&iter).size_hint();
    let mut acc = hint.0 ^ hint.1.unwrap_or(0);
    for byte in iter {
        acc ^= byte as usize;
    }
    black_box(acc)
}

fn main() {
    let carrier = Carrier {
        buf: vec![1, 2, 3, 4, 5, 6],
    };
    let out = consume(carrier);
    black_box(out);
}
