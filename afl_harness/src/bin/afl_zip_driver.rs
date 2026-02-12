use std::io::{Cursor, Read};

use zip::read::ZipArchive;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let cursor = Cursor::new(data);
    let mut archive = match ZipArchive::new(cursor) {
        Ok(archive) => archive,
        Err(_) => return,
    };

    let file_count = archive.len().min(16);
    for i in 0..file_count {
        let mut file = match archive.by_index(i) {
            Ok(file) => file,
            Err(_) => continue,
        };

        // Bound total extracted bytes per file to keep fuzz iterations stable.
        let mut left = 64 * 1024usize;
        let mut buf = [0u8; 4096];
        while left > 0 {
            let take = left.min(buf.len());
            match file.read(&mut buf[..take]) {
                Ok(0) => break,
                Ok(n) => left = left.saturating_sub(n),
                Err(_) => break,
            }
        }
    }
}
