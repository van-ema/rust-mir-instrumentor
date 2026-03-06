use std::hint::black_box;
use toml::Value;

fn main() {
    let iters = eco_bench::parse_iters(200);
    let input = r#"
name = "rusteze"
enabled = true
retry = 3
hosts = ["a.example", "b.example"]
"#;
    let mut total = 0usize;

    for i in 0..iters {
        let mut cfg: Value = toml::from_str(input).expect("toml parse failed");
        let retry = cfg.get("retry").and_then(Value::as_integer).unwrap_or(0);
        cfg["retry"] = Value::Integer(retry.saturating_add((i & 0xff) as i64));
        let out = toml::to_string(&cfg).expect("toml serialize failed");
        total ^= out.len();
    }

    black_box(total);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{total}");
}
