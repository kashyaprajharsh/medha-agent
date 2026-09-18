//! Byte-level image admission, shared by every surface and tool that can hand
//! Medha an image: identify it, normalise it to a form every supported
//! endpoint accepts, and keep it inside the transmission budget.

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation};
use std::io::Cursor;

pub const MAX_BYTES: usize = kernel::artifacts::MAX_IMAGE_BYTES;

/// Decode guard: a header claiming more pixels than this is refused before any
/// buffer is allocated for it.
const MAX_PIXELS: u64 = 50_000_000;

/// First long-edge target when an image has to shrink to fit the budget. Well
/// above what vision models sample at, so screenshot text stays readable.
const LONG_EDGE: u32 = 2048;
const MIN_LONG_EDGE: u32 = 256;
const JPEG_QUALITY: u8 = 85;

pub struct Image {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    /// What admission changed, for the surface to show. `None` = sent verbatim.
    pub note: Option<String>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Image")
            .field("mime", &self.mime)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes.len())
            .field("note", &self.note)
            .finish()
    }
}

struct Kind {
    mime: &'static str,
    format: ImageFormat,
    /// Accepted as-is by every protocol Medha can lower media to.
    universal: bool,
}

pub fn normalize(raw: Vec<u8>) -> Result<Image> {
    within(raw, MAX_BYTES)
}

fn within(raw: Vec<u8>, budget: usize) -> Result<Image> {
    let kind = identify(&raw)?;
    let (width, height, orientation) = probe(&raw, kind.format)?;
    // Decode even when the bytes will be forwarded untouched: a truncated file
    // has to fail here, with the path in the message, rather than as a provider
    // rejection halfway through a turn.
    let mut image = DynamicImage::from_decoder(reader(&raw, kind.format).into_decoder()?)
        .with_context(|| format!("this {} is damaged or truncated", kind.mime))?;
    if kind.universal && orientation == Orientation::NoTransforms && raw.len() <= budget {
        return Ok(Image {
            bytes: raw,
            mime: kind.mime,
            width,
            height,
            note: None,
        });
    }

    let mut notes = Vec::new();
    if orientation != Orientation::NoTransforms {
        image.apply_orientation(orientation);
        notes.push("rotated to its EXIF orientation".to_string());
    }
    let (upright_width, upright_height) = (image.width(), image.height());
    let fitted = fit_budget(image, kind.format == ImageFormat::Jpeg, budget)?;
    if fitted.mime != kind.mime {
        notes.push(format!("{} → {}", kind.mime, fitted.mime));
    }
    if (fitted.width, fitted.height) != (upright_width, upright_height) {
        notes.push(format!(
            "resized {upright_width}×{upright_height} → {}×{}",
            fitted.width, fitted.height
        ));
    }
    Ok(Image {
        note: (!notes.is_empty()).then(|| notes.join("; ")),
        ..fitted
    })
}

/// Encode at native size first and shrink only when the result does not fit, so
/// an image that is already small pays no quality tax. A PNG that stays too
/// large retries as JPEG before any more resolution is given up.
fn fit_budget(image: DynamicImage, prefer_jpeg: bool, budget: usize) -> Result<Image> {
    let formats: &[ImageFormat] = if prefer_jpeg {
        &[ImageFormat::Jpeg]
    } else {
        &[ImageFormat::Png, ImageFormat::Jpeg]
    };
    let mut current = image;
    loop {
        for format in formats {
            let bytes = encode(&current, *format)?;
            if bytes.len() <= budget {
                return Ok(Image {
                    bytes,
                    mime: mime_of(*format),
                    width: current.width(),
                    height: current.height(),
                    note: None,
                });
            }
        }
        let long_edge = current.width().max(current.height());
        let next = if long_edge > LONG_EDGE {
            LONG_EDGE
        } else {
            long_edge / 2
        };
        ensure!(
            next >= MIN_LONG_EDGE,
            "image cannot be reduced to fit {budget} bytes"
        );
        current = current.resize(next, next, image::imageops::FilterType::Lanczos3);
    }
}

/// The clipboard hands over raw pixels rather than an encoded file.
pub fn from_rgba(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>> {
    let buffer = image::RgbaImage::from_raw(width, height, pixels.to_vec())
        .context("clipboard pixel buffer does not match its reported size")?;
    encode(&DynamicImage::ImageRgba8(buffer), ImageFormat::Png)
}

fn mime_of(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        _ => "image/png",
    }
}

fn encode(image: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match format {
        ImageFormat::Jpeg => image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut Cursor::new(&mut bytes),
            JPEG_QUALITY,
        )
        .encode_image(&image.to_rgb8())?,
        _ => image.write_to(&mut Cursor::new(&mut bytes), format)?,
    }
    Ok(bytes)
}

/// Dimensions and orientation come from the header, before anything is decoded.
fn probe(raw: &[u8], format: ImageFormat) -> Result<(u32, u32, Orientation)> {
    let mut decoder = reader(raw, format).into_decoder()?;
    let (width, height) = decoder.dimensions();
    ensure!(width > 0 && height > 0, "image has no pixels");
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "image is {width}×{height}; Medha refuses to decode more than {MAX_PIXELS} pixels"
    );
    Ok((
        width,
        height,
        decoder.orientation().unwrap_or(Orientation::NoTransforms),
    ))
}

fn reader(raw: &[u8], format: ImageFormat) -> ImageReader<Cursor<&[u8]>> {
    let mut limits = image::Limits::no_limits();
    limits.max_alloc = Some(512 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(raw), format);
    reader.limits(limits);
    reader
}

/// Content decides the format, never a caller-supplied extension.
fn identify(raw: &[u8]) -> Result<Kind> {
    let boxed = raw.get(4..8) == Some(b"ftyp");
    let (mime, format, universal) = match raw {
        _ if raw.starts_with(b"\x89PNG\r\n\x1a\n") => ("image/png", ImageFormat::Png, true),
        _ if raw.starts_with(b"\xff\xd8\xff") => ("image/jpeg", ImageFormat::Jpeg, true),
        _ if raw.starts_with(b"GIF87a") || raw.starts_with(b"GIF89a") => {
            ("image/gif", ImageFormat::Gif, true)
        }
        _ if raw.starts_with(b"RIFF") && raw.get(8..12) == Some(b"WEBP") => {
            ("image/webp", ImageFormat::WebP, true)
        }
        _ if raw.starts_with(b"BM") => ("image/bmp", ImageFormat::Bmp, false),
        _ if raw.starts_with(b"II*\x00") || raw.starts_with(b"MM\x00*") => {
            ("image/tiff", ImageFormat::Tiff, false)
        }
        _ if raw.starts_with(b"\x00\x00\x01\x00") => ("image/x-icon", ImageFormat::Ico, false),
        // Decoding these needs a C library Medha does not link, so name the
        // format instead of reporting a corrupt file.
        _ if boxed && matches!(raw.get(8..12), Some(b"avif" | b"avis")) => {
            bail!("AVIF images are not supported yet; convert to PNG or JPEG first")
        }
        _ if boxed => bail!("HEIC/HEIF images are not supported yet; export as PNG or JPEG first"),
        _ if raw.starts_with(b"%PDF-") => {
            bail!("PDFs are not image attachments; render the page you need as PNG first")
        }
        _ => bail!(
            "expected a PNG, JPEG, WebP, GIF, BMP, TIFF, or ICO image; empty files and other formats are not supported"
        ),
    };
    Ok(Kind {
        mime,
        format,
        universal,
    })
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
