use quick_xml::events::Event;
use quick_xml::Reader;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut reader = Reader::from_reader(std::io::Cursor::new(&data));
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
}
