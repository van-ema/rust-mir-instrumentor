use image::imageops::FilterType;
use image::ImageFormat;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut bounded = data;
    if bounded.len() > 1 << 20 {
        bounded.truncate(1 << 20);
    }

    let format = image::guess_format(&bounded).unwrap_or(ImageFormat::Png);
    let Ok(img) = image::load_from_memory_with_format(&bounded, format) else {
        return;
    };

    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let target_w = w.clamp(1, 64);
    let target_h = h.clamp(1, 64);

    let resized = image::imageops::resize(&rgb, target_w, target_h, FilterType::Triangle);
    let flipped = image::imageops::flip_horizontal(&resized);
    let checksum = flipped
        .as_raw()
        .iter()
        .fold(0u64, |acc, &b| acc.wrapping_mul(131).wrapping_add(u64::from(b)));
    std::hint::black_box(checksum);
}
