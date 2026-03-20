use image::ImageFormat;
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(50);
    let input = include_bytes!("../../data/eco_image_input.png");
    let mut total = 0usize;

    for i in 0..iters {
        let img = image::load_from_memory_with_format(input, ImageFormat::Png)
            .expect("png decode failed");
        let rgb = img.to_rgb8();
        let p = rgb.get_pixel((i as u32) % 8, ((i as u32) * 3) % 8).0;
        total ^= rgb.width() as usize;
        total ^= rgb.height() as usize;
        total ^= usize::from(p[0]) ^ usize::from(p[1]) ^ usize::from(p[2]);
    }

    black_box(total);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{total}");
}
