//! Profile pictures: a chosen image becomes a small square JPEG, and received ones become pixels.

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Side of the square picture that is sent to the servers.
const SIZE: u32 = 128;
/// What the servers accept.
const MAX_LEN: usize = 24 * 1024;
/// Pictures bigger than this are not even opened.
const MAX_FILE: u64 = 32 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PictureError {
    TooBig,
    Unreadable,
}

/// Reads an image file and turns it into the picture to send.
pub fn prepare(path: &std::path::Path) -> Result<Vec<u8>, PictureError> {
    let size = std::fs::metadata(path).map_err(|_| PictureError::Unreadable)?.len();
    if size > MAX_FILE {
        return Err(PictureError::TooBig);
    }
    let bytes = std::fs::read(path).map_err(|_| PictureError::Unreadable)?;
    encode(&image::load_from_memory(&bytes).map_err(|_| PictureError::Unreadable)?)
}

/// Crops the middle square, scales it down and encodes it as JPEG small enough for the servers.
fn encode(image: &DynamicImage) -> Result<Vec<u8>, PictureError> {
    let (width, height) = image.dimensions();
    let side = width.min(height).max(1);
    let square = image.crop_imm((width - side) / 2, (height - side) / 2, side, side);
    let small = square.resize_exact(SIZE, SIZE, FilterType::CatmullRom).to_rgba8();
    // JPEG has no transparency: transparent parts become white.
    let flat = RgbImage::from_fn(SIZE, SIZE, |x, y| {
        let [r, g, b, a] = small.get_pixel(x, y).0;
        let blend = |channel: u8| ((u32::from(channel) * u32::from(a) + 255 * (255 - u32::from(a))) / 255) as u8;
        Rgb([blend(r), blend(g), blend(b)])
    });
    for quality in [88, 75, 60, 45] {
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, quality).encode_image(&flat).map_err(|_| PictureError::Unreadable)?;
        if out.len() <= MAX_LEN {
            return Ok(out);
        }
    }
    Err(PictureError::TooBig)
}

/// Decodes a received picture for showing it.
pub fn pixels(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_a_small_square_jpeg() {
        let wide = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(900, 300, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, if x < 450 { 255 } else { 0 }])
        }));
        let jpeg = encode(&wide).unwrap();
        assert!(jpeg.len() <= MAX_LEN);
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]));
        let decoded = pixels(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (SIZE, SIZE));
        assert!(pixels(b"not a picture").is_none());
    }
}
