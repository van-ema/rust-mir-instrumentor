macro_rules! name {
    ($($tt:tt)*) => {{}};
}

macro_rules! alloc_id {
    ($($tt:tt)*) => {{
        0usize
    }};
}

macro_rules! print_state {
    ($($tt:tt)*) => {{}};
}
