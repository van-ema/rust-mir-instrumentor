const MAX_TOTAL_LEN: usize = 256;

#[derive(Clone)]
struct OracleVec {
    buf: [u8; MAX_TOTAL_LEN],
    len: usize,
}

impl Default for OracleVec {
    fn default() -> Self {
        Self {
            buf: [0; MAX_TOTAL_LEN],
            len: 0,
        }
    }
}

impl OracleVec {
    fn extend_from_slice(&mut self, slice: &[u8]) {
        let room = MAX_TOTAL_LEN.saturating_sub(self.len);
        let count = slice.len().min(room);
        self.buf[self.len..self.len + count].copy_from_slice(&slice[..count]);
        self.len += count;
    }

    // Original harness code that triggered the TB-lite violation.
    fn append_old(&mut self, other: &mut Self) {
        let room = MAX_TOTAL_LEN.saturating_sub(self.len);
        let other_len = other.len;
        let count = other_len.min(room);
        let mut tmp = [0u8; MAX_TOTAL_LEN];
        tmp[..count].copy_from_slice(&other.buf[..count]);
        self.extend_from_slice(&tmp[..count]);
        if count < other_len {
            other.buf.copy_within(count..other_len, 0);
        }
        other.len = other_len - count;
    }
}

fn main() {
    let mut dst = OracleVec::default();
    dst.extend_from_slice(&[1, 2, 3, 4]);

    let mut src = OracleVec::default();
    src.extend_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);

    // Keep calling the old implementation on a stable shape so the
    // instrumentation/runtime behavior is easy to inspect.
    for _ in 0..32 {
        dst.append_old(&mut src);
        if src.len == 0 {
            src.extend_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);
        }
        if dst.len > 64 {
            dst = OracleVec::default();
            dst.extend_from_slice(&[1, 2, 3, 4]);
        }
    }
}
