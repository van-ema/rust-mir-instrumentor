use rkyv::{Archive, Deserialize, Serialize};

#[derive(Archive, Deserialize, Serialize, Debug)]
struct FuzzRecord {
    tag: u32,
    bytes: Vec<u8>,
    text: String,
}

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let _ = rkyv::from_bytes::<FuzzRecord, rkyv::rancor::Error>(&data);

    let keep = data.len().min(128);
    let record = FuzzRecord {
        tag: keep as u32,
        bytes: data[..keep].to_vec(),
        text: String::from_utf8_lossy(&data[..keep]).into_owned(),
    };

    if let Ok(bytes) = rkyv::to_bytes::<rkyv::rancor::Error>(&record) {
        let _ = rkyv::from_bytes::<FuzzRecord, rkyv::rancor::Error>(&bytes);
    }
}
