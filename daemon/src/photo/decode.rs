//! Memory-bounded image decoding.
//!
//! Hashing, thumbnails and captions all need far fewer pixels than a phone
//! camera produces: a 48 MP JPEG is ~140 MB of RGB when fully decoded. JPEGs
//! (almost every photo) are therefore decoded straight at reduced scale in the
//! DCT domain (1/2, 1/4 or 1/8), so a large photo costs a few MB. Other
//! formats fall back to a full decode under a hard allocation ceiling.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};

use image::{DynamicImage, GrayImage, ImageReader, Limits, RgbImage};

/// Refuse pathological inputs outright.
const MAX_DIMENSION: u32 = 20_000;
/// Ceiling for a full (non-JPEG) decode. PNG/WebP photos this large are rare;
/// screenshots are far smaller.
const MAX_ALLOC: u64 = 256 * 1024 * 1024;

/// Decode `bytes` with its shorter useful side at least `min_side` px where
/// the source allows, using as little memory as the format permits. `None`
/// for undecodable input (e.g. HEIC, which has no pure-Rust decoder).
pub fn decode_at_least(bytes: &[u8], min_side: u32) -> Option<DynamicImage> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // A malformed JPEG can make the decoder panic; treat that as
        // undecodable rather than taking the worker down.
        if let Ok(Some(img)) =
            catch_unwind(AssertUnwindSafe(|| decode_jpeg_scaled(bytes, min_side)))
        {
            return Some(img);
        }
    }
    decode_full(bytes)
}

/// Re-encode `bytes` as a JPEG at most `max_side` px on its long side, never
/// upscaled. Used for thumbnails and for the copies sent to a vision model.
pub fn render_jpeg(bytes: &[u8], max_side: u32, quality: u8) -> Option<Vec<u8>> {
    let img = decode_at_least(bytes, max_side)?;
    let img = if img.width().max(img.height()) > max_side {
        img.thumbnail(max_side, max_side)
    } else {
        img
    };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&img.to_rgb8())
        .ok()?;
    Some(out)
}

fn decode_jpeg_scaled(bytes: &[u8], min_side: u32) -> Option<DynamicImage> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    decoder.set_max_decoding_buffer_size(MAX_ALLOC as usize);
    let side = u16::try_from(min_side).unwrap_or(u16::MAX);
    decoder.scale(side, side).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            GrayImage::from_raw(w, h, pixels).map(DynamicImage::ImageLuma8)
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            RgbImage::from_raw(w, h, pixels).map(DynamicImage::ImageRgb8)
        }
        // 16-bit and CMYK JPEGs are rare: let the general decoder handle them.
        _ => None,
    }
}

fn decode_full(bytes: &[u8]) -> Option<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    reader.decode().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageBuffer, Rgb};

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let img: RgbImage = ImageBuffer::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&img)
            .unwrap();
        out
    }

    #[test]
    fn large_jpegs_are_decoded_at_reduced_scale() {
        let img = decode_at_least(&jpeg(4096, 3072), 256).expect("decodes");
        // 1/8 scale is the smallest that keeps a side >= 256.
        assert_eq!((img.width(), img.height()), (512, 384));
    }

    #[test]
    fn small_jpegs_are_not_upscaled_or_rejected() {
        let img = decode_at_least(&jpeg(200, 100), 256).expect("decodes");
        assert_eq!((img.width(), img.height()), (200, 100));
    }

    #[test]
    fn non_jpeg_formats_use_the_general_decoder() {
        let png: Vec<u8> = {
            let img: RgbImage = ImageBuffer::from_pixel(64, 32, Rgb([1, 2, 3]));
            let mut out = Vec::new();
            DynamicImage::ImageRgb8(img)
                .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            out
        };
        let img = decode_at_least(&png, 256).expect("decodes");
        assert_eq!((img.width(), img.height()), (64, 32));
    }

    #[test]
    fn render_jpeg_bounds_the_long_side() {
        let out = render_jpeg(&jpeg(4096, 3072), 256, 80).expect("renders");
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!(img.width().max(img.height()), 256);
    }

    #[test]
    fn garbage_is_undecodable_not_a_panic() {
        assert!(decode_at_least(&[0xFF, 0xD8, 0xFF, 0x00, 0x01], 256).is_none());
        assert!(decode_at_least(b"not an image", 256).is_none());
    }
}
