use quick_xml::events::Event;
use quick_xml::Reader;
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(1_000);
    let input = r#"<root><entry id="1">alpha</entry><entry id="2">beta</entry></root>"#;
    let mut total = 0usize;

    for _ in 0..iters {
        let mut reader = Reader::from_str(input);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut tags = 0usize;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(_)) => tags += 1,
                Ok(Event::Empty(_)) => tags += 1,
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("xml parse failed: {e}"),
            }
            buf.clear();
        }

        total ^= tags;
    }

    black_box(total);
    println!("{total}");
}
