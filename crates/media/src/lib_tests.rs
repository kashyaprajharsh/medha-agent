use super::*;
use image::{ImageBuffer, Rgba};

fn canvas(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
        Rgba([(x % 256) as u8, (y % 256) as u8, 40, 255])
    }))
}

fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    encode(image, format).unwrap()
}

#[test]
fn an_acceptable_image_is_sent_byte_for_byte() {
    let png = encoded(&canvas(8, 6), ImageFormat::Png);
    let admitted = normalize(png.clone()).unwrap();
    assert_eq!(admitted.bytes, png);
    assert_eq!(admitted.mime, "image/png");
    assert_eq!((admitted.width, admitted.height), (8, 6));
    assert!(admitted.note.is_none());
}

#[test]
fn formats_no_endpoint_accepts_are_converted_and_reported() {
    for (format, mime) in [
        (ImageFormat::Bmp, "image/bmp"),
        (ImageFormat::Tiff, "image/tiff"),
    ] {
        let admitted = normalize(encoded(&canvas(10, 4), format)).unwrap();
        assert_eq!(admitted.mime, "image/png");
        assert_eq!((admitted.width, admitted.height), (10, 4));
        assert!(
            admitted.note.as_deref().unwrap().contains(mime),
            "{:?}",
            admitted.note
        );
    }
}

#[test]
fn an_oversized_image_is_resized_rather_than_refused() {
    let raw = encoded(&canvas(3000, 1500), ImageFormat::Png);
    let budget = raw.len() / 2;
    let admitted = within(raw, budget).unwrap();
    assert!(admitted.bytes.len() <= budget, "{}", admitted.bytes.len());
    assert!(admitted.width.max(admitted.height) <= LONG_EDGE);
    assert_eq!(admitted.width, admitted.height * 2, "aspect ratio kept");
    let note = admitted.note.unwrap();
    assert!(note.contains("resized 3000×1500 → "), "{note}");
}

#[test]
fn a_photo_that_stays_too_large_as_png_is_encoded_as_jpeg() {
    let noisy = DynamicImage::ImageRgba8(ImageBuffer::from_fn(600, 600, |x, y| {
        let seed = x
            .wrapping_mul(2_654_435_761)
            .wrapping_add(y.wrapping_mul(40_503));
        Rgba([
            (seed >> 3) as u8,
            (seed >> 11) as u8,
            (seed >> 19) as u8,
            255,
        ])
    }));
    let admitted = within(encoded(&noisy, ImageFormat::Png), 200_000).unwrap();
    assert_eq!(admitted.mime, "image/jpeg");
    assert!(admitted.bytes.len() <= 200_000);
    assert!(admitted.note.unwrap().contains("image/png → image/jpeg"));
}

#[test]
fn a_rotated_photo_is_made_upright_before_it_is_sent() {
    let jpeg = with_exif_orientation(&encoded(&canvas(40, 20), ImageFormat::Jpeg), 6);
    let admitted = normalize(jpeg).unwrap();
    assert_eq!((admitted.width, admitted.height), (20, 40));
    assert!(admitted.note.unwrap().contains("EXIF orientation"));
}

#[test]
fn unsupported_formats_say_which_format_they_are() {
    let heic = [b"\0\0\0\x18".as_slice(), b"ftyp", b"heic", b"rest"].concat();
    let avif = [b"\0\0\0\x18".as_slice(), b"ftyp", b"avif", b"rest"].concat();
    for (bytes, expected) in [
        (heic, "HEIC/HEIF"),
        (avif, "AVIF"),
        (b"%PDF-1.7 rest".to_vec(), "PDF"),
        (b"<svg></svg>".to_vec(), "expected a PNG"),
        (Vec::new(), "expected a PNG"),
    ] {
        let error = normalize(bytes).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn a_truncated_image_is_refused_rather_than_half_decoded() {
    let mut png = encoded(&canvas(32, 32), ImageFormat::Png);
    png.truncate(png.len() / 2);
    assert!(normalize(png).is_err());
}

#[test]
fn a_header_claiming_an_impossible_size_is_refused_before_allocating() {
    let mut header = encoded(&canvas(4, 4), ImageFormat::Bmp);
    header[18..22].copy_from_slice(&40_000i32.to_le_bytes());
    header[22..26].copy_from_slice(&40_000i32.to_le_bytes());
    let error = normalize(header).unwrap_err().to_string();
    assert!(error.contains("refuses to decode"), "{error}");
}

/// Splice a minimal Exif APP1 segment carrying one orientation tag, the way a
/// phone camera writes it.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::from(*b"II*\x00");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&0u16.to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let mut app1 = Vec::from(*b"Exif\x00\x00");
    app1.extend_from_slice(&tiff);
    let mut out = Vec::from(&jpeg[..2]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}
