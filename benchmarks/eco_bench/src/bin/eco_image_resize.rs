use image::imageops::FilterType;
use image::ImageFormat;
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(1);
    let input = include_bytes!("../../data/eco_image_input.png");
    let mut total = 0usize;

    for i in 0..iters {
        let img = image::load_from_memory_with_format(input, ImageFormat::Png)
            .expect("png decode failed");
        let resized = img.resize_exact(8 + (i % 3) as u32, 8, FilterType::Triangle);
        let thumb = resized.thumbnail_exact(4, 4).to_rgba8();
        let p = thumb.get_pixel((i as u32) % 4, ((i as u32) * 3) % 4).0;
        total ^= thumb.width() as usize;
        total ^= thumb.height() as usize;
        total ^= usize::from(p[0]) ^ usize::from(p[1]) ^ usize::from(p[2]) ^ usize::from(p[3]);
    }

    black_box(total);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{total}");
}
