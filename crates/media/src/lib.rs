//! Byte-level image admission, shared by every surface and tool that can hand
//! Medha an image: identify it, normalise it to a form every supported
//! endpoint accepts, and keep it inside the transmission budget.

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation};
use std::io::{Cursor, Read};

pub const MAX_BYTES: usize = kernel::artifacts::MAX_IMAGE_BYTES;
pub const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

/// A shared bounded reader for user paths and model-callable image tools.
pub fn read_source(path: &std::path::Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open attachment {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "attachment must be a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_SOURCE_BYTES as u64,
        "attachment is larger than {MAX_SOURCE_BYTES} bytes"
    );
    let mut raw = Vec::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() <= MAX_SOURCE_BYTES,
        "attachment is larger than {MAX_SOURCE_BYTES} bytes"
    );
    Ok(raw)
}

/// Recognize an image header without allocating a decoded pixel buffer.
/// Full validation and size guards still belong to `normalize`.
pub fn has_image_header(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok()
}

/// Decode guard: a header claiming more pixels than this is refused before any
/// buffer is allocated for it.
const MAX_PIXELS: u64 = 50_000_000;
const MAX_DIMENSION: u32 = 16_384;

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

/// Local admission ceilings. Operators can lower them with environment
/// variables; the artifact and request path still enforce the 10 MiB hard cap.
#[derive(Debug, Clone, Copy)]
pub struct ImageLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
    pub max_encoded_bytes: usize,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_width: MAX_DIMENSION,
            max_height: MAX_DIMENSION,
            max_pixels: MAX_PIXELS,
            max_encoded_bytes: MAX_BYTES,
        }
    }
}

impl ImageLimits {
    pub fn from_env() -> Result<Self> {
        fn ceiling<T: std::str::FromStr + PartialOrd + Copy + Default>(
            name: &str,
            default: T,
            hard_max: T,
        ) -> Result<T> {
            let Some(value) = std::env::var_os(name) else {
                return Ok(default);
            };
            let text = value.to_str().context(format!("{name} must be UTF-8"))?;
            let parsed = text
                .parse::<T>()
                .map_err(|_| anyhow::anyhow!("invalid {name}"))?;
            ensure!(
                parsed > T::default() && parsed <= hard_max,
                "{name} exceeds its allowed range"
            );
            Ok(parsed)
        }
        // Parse as integers here instead of accepting strings with units, so
        // the exact byte and pixel ceiling is clear in diagnostics.
        Ok(Self {
            max_width: ceiling("MEDHA_IMAGE_MAX_WIDTH", MAX_DIMENSION, MAX_DIMENSION)?,
            max_height: ceiling("MEDHA_IMAGE_MAX_HEIGHT", MAX_DIMENSION, MAX_DIMENSION)?,
            max_pixels: ceiling("MEDHA_IMAGE_MAX_PIXELS", MAX_PIXELS, MAX_PIXELS)?,
            max_encoded_bytes: ceiling("MEDHA_IMAGE_MAX_BYTES", MAX_BYTES, MAX_BYTES)?,
        })
    }
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

/// Whether a path is *named* like an image. A hint for routing a read, never a
/// decision about content: admission still identifies the bytes, so a file with
/// a lying extension gets a clear error rather than a wrong answer.
pub fn has_image_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "tif"
                    | "tiff"
                    | "ico"
                    | "heic"
                    | "heif"
                    | "avif"
            )
        })
}

pub fn normalize(raw: Vec<u8>) -> Result<Image> {
    let limits = ImageLimits::from_env()?;
    within_limits(raw, limits.max_encoded_bytes, limits)
}

/// Normalize through the same admission path while respecting a surface's
/// smaller transport budget. It can only lower configured safety ceilings.
pub fn normalize_for_transport(raw: Vec<u8>, budget: usize) -> Result<Image> {
    ensure!(raw.len() <= MAX_SOURCE_BYTES, "image source exceeds 64 MiB");
    ensure!(budget > 0, "image transport budget must be positive");
    let limits = ImageLimits::from_env()?;
    within_limits(raw, budget.min(limits.max_encoded_bytes), limits)
}

#[cfg(test)]
fn within(raw: Vec<u8>, budget: usize) -> Result<Image> {
    within_limits(raw, budget, ImageLimits::default())
}

fn within_limits(raw: Vec<u8>, budget: usize, limits: ImageLimits) -> Result<Image> {
    let kind = identify(&raw)?;
    let (width, height, orientation) = probe(&raw, kind.format, limits)?;
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
    let limits = ImageLimits::from_env()?;
    ensure!(
        width <= limits.max_width && height <= limits.max_height,
        "clipboard image dimensions exceed the configured limit"
    );
    ensure!(
        u64::from(width) * u64::from(height) <= limits.max_pixels,
        "clipboard image pixel count exceeds the configured limit"
    );
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
fn probe(raw: &[u8], format: ImageFormat, limits: ImageLimits) -> Result<(u32, u32, Orientation)> {
    let mut decoder = reader(raw, format).into_decoder()?;
    let (width, height) = decoder.dimensions();
    ensure!(width > 0 && height > 0, "image has no pixels");
    ensure!(
        width <= limits.max_width && height <= limits.max_height,
        "image is {width}×{height}; configured dimension limit is {}×{}",
        limits.max_width,
        limits.max_height
    );
    ensure!(
        u64::from(width) * u64::from(height) <= limits.max_pixels,
        "image is {width}×{height}; Medha refuses to decode more than {} pixels",
        limits.max_pixels
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
            // Gemini Interactions does not accept GIF. Decode the first frame
            // and normalize it to PNG so every implemented protocol receives
            // a MIME type its adapter can lower.
            ("image/gif", ImageFormat::Gif, false)
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
