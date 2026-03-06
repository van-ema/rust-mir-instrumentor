use std::hint::black_box;
use serde_json::Value;

fn main() {
    let iters = eco_bench::parse_iters(300);
    let input = r#"{"id":42,"label":"rusteze","values":[1,2,3,5,8,13]}"#;
    let mut total = 0usize;

    for i in 0..iters {
        let mut item: Value = serde_json::from_str(input).expect("json parse failed");
        item["id"] = Value::from(item["id"].as_u64().unwrap_or(0).wrapping_add(i));
        let mut values = item["values"].as_array().cloned().unwrap_or_default();
        values.push(Value::from((i & 0xffff) as u32));
        item["values"] = Value::from(values);
        let out = serde_json::to_string(&item).expect("json serialize failed");
        total ^= out.len();
    }

    black_box(total);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{total}");
}
