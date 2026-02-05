use smallvec::SmallVec;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut v: SmallVec<u8, 8> = SmallVec::new();
    let mut idx = 0usize;
    let mut steps = 0usize;

    // Keep per-input work bounded so AFL doesn't get stuck on quadratic behaviors.
    let max_steps: usize = std::env::var("AFL_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let max_len: usize = std::env::var("AFL_MAX_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_096);

    while idx < data.len() && steps < max_steps {
        let op = data[idx] % 6;
        idx += 1;
        steps += 1;

        match op {
            0 => {
                if idx < data.len() {
                    if v.len() < max_len {
                        v.push(data[idx]);
                    }
                    idx += 1;
                }
            }
            1 => {
                let _ = v.pop();
            }
            2 => {
                if idx < data.len() {
                    if v.len() >= max_len {
                        // Avoid unbounded growth via inserts.
                        idx = idx.saturating_add(2).min(data.len());
                        continue;
                    }
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
                        if v.len() < max_len {
                            let room = max_len - v.len();
                            let actual_end = idx + (end - idx).min(room);
                            v.extend_from_slice(&data[idx..actual_end]);
                        }
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
