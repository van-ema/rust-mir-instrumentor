use smallvec::SmallVec;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut v: SmallVec<u8, 8> = SmallVec::new();
    let mut idx = 0usize;

    while idx < data.len() {
        let op = data[idx] % 6;
        idx += 1;

        match op {
            0 => {
                if idx < data.len() {
                    v.push(data[idx]);
                    idx += 1;
                }
            }
            1 => {
                let _ = v.pop();
            }
            2 => {
                if idx < data.len() {
                    let pos = (data[idx] as usize) % (v.len() + 1);
                    idx += 1;
                    let val = if idx < data.len() { data[idx] } else { 0 };
                    if idx < data.len() {
                        idx += 1;
                    }
                    v.insert(pos, val);
                }
            }
            3 => {
                if !v.is_empty() {
                    let pos = (data[idx.saturating_sub(1)] as usize) % v.len();
                    v.remove(pos);
                }
            }
            4 => {
                if idx < data.len() {
                    let len = (data[idx] as usize).min(16);
                    idx += 1;
                    let end = (idx + len).min(data.len());
                    if end > idx {
                        v.extend_from_slice(&data[idx..end]);
                        idx = end;
                    }
                }
            }
            _ => {
                let sum: u32 = v.iter().map(|&x| x as u32).sum();
                if sum == 0xFFFF_FFFF {
                    std::hint::black_box(sum);
                }
            }
        }
    }

    let _ = v.len();
}
