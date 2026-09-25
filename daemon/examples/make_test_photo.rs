//! Writes a synthetic JPEG of a given size, for memory tests that need
//! camera-sized photos without shipping binary fixtures.
//!
//! Usage: cargo run --example make_test_photo -- OUT.jpg WIDTH HEIGHT [SEED]

use image::{ImageBuffer, Rgb};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: make_test_photo OUT.jpg WIDTH HEIGHT [SEED]");
        std::process::exit(2);
    }
    let width: u32 = args[2].parse().expect("width");
    let height: u32 = args[3].parse().expect("height");
    let seed: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
    // Smooth gradients plus a seed-dependent pattern: distinct photos hash
    // differently, and the file compresses like a real photo rather than noise.
    let img = ImageBuffer::from_fn(width, height, |x, y| {
        let v = (x.wrapping_mul(seed) ^ y.wrapping_mul(seed.wrapping_add(7))) % 256;
        Rgb([(x % 256) as u8, (y % 256) as u8, v as u8])
    });
    let file = std::fs::File::create(&args[1]).expect("create output");
    let mut out = std::io::BufWriter::new(file);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&img)
        .expect("encode");
}
