//! Immutable local transport staging. Only the backend persists artifacts.
use super::input::ImageSource;
use base64::Engine;
use std::path::{Path, PathBuf};

pub(super) struct Image {
    pub wire: protocol::Image,
    source: Option<PathBuf>,
    width: u32,
    height: u32,
    bytes: usize,
}
impl Image {
    fn summary(&self) -> String {
        format!(
            "{}  {}×{} · {}",
            self.wire.name,
            self.width,
            self.height,
            crate::attachments::human_size(self.bytes)
        )
    }
}

#[derive(Default)]
pub(super) struct Pending {
    staged: Vec<Image>,
    loading: bool,
    generation: u64,
}
impl Pending {
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }
    pub fn holds(&self, path: &Path) -> bool {
        self.staged
            .iter()
            .any(|image| image.source.as_deref() == Some(path))
    }
    pub fn begin(&mut self) -> Result<u64, String> {
        if self.loading {
            return Err("An image is still loading — wait for it to finish.".into());
        }
        if self.staged.len() >= 4 {
            return Err("Four images are already attached — remove one first.".into());
        }
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        Ok(self.generation)
    }
    pub fn accept(&mut self, generation: u64, images: Vec<Image>) -> Result<String, String> {
        if generation != self.generation {
            return Err("The attachment belongs to an earlier composer.".into());
        }
        self.loading = false;
        if self.staged.len() + images.len() > 4 {
            return Err("Attach at most four images per message.".into());
        }
        let notice = images
            .iter()
            .map(|image| match &image.wire.note {
                Some(note) => format!("attached {} ({note})", image.summary()),
                None => format!("attached {}", image.summary()),
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.staged.extend(images);
        Ok(format!(
            "{notice}\ntype your message and press Enter, or /attach remove"
        ))
    }
    pub fn failed(&mut self, generation: u64) {
        if generation == self.generation {
            self.loading = false;
        }
    }
    pub fn reset(&mut self) {
        self.staged.clear();
        self.loading = false;
        self.generation = self.generation.wrapping_add(1);
    }
    pub fn detach(&mut self, arg: &str) -> String {
        let arg = arg.trim();
        if arg.is_empty() || arg == "all" {
            self.reset();
            return "attachments cleared".into();
        }
        match arg.parse::<usize>() {
            Ok(index) if (1..=self.staged.len()).contains(&index) => {
                let image = self.staged.remove(index - 1);
                format!(
                    "detached {} ({} staged)",
                    image.wire.name,
                    self.staged.len()
                )
            }
            Ok(_) => "no attachment at that number".into(),
            Err(_) => "usage: /attach remove [number|all]".into(),
        }
    }
    pub fn chips(&self) -> Vec<String> {
        let mut rows: Vec<_> = self
            .staged
            .iter()
            .enumerate()
            .map(|(index, image)| format!("[{} ×] {}", index + 1, image.summary()))
            .collect();
        if self.loading {
            rows.push("[⋯] attaching image…".into());
        }
        rows
    }
    pub fn wire(&self) -> Vec<protocol::Image> {
        self.staged.iter().map(|image| image.wire.clone()).collect()
    }
    pub fn restore(&mut self, images: Vec<Image>) {
        self.reset();
        self.staged = images;
    }
}

fn admit(raw: Vec<u8>, name: String, source: Option<PathBuf>) -> Result<Image, String> {
    let image =
        media::normalize_for_transport(raw, 2 * 1024 * 1024).map_err(|error| error.to_string())?;
    Ok(Image {
        wire: protocol::Image {
            id: ulid::Ulid::new().to_string(),
            name,
            mime: image.mime.into(),
            data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
            note: image.note,
        },
        source,
        width: image.width,
        height: image.height,
        bytes: image.bytes.len(),
    })
}

pub(super) async fn load(source: &ImageSource) -> Result<Vec<Image>, String> {
    let paths = match source {
        ImageSource::Paths(paths) => Some(paths.clone()),
        ImageSource::Clipboard => None,
    };
    tokio::task::spawn_blocking(move || match paths {
        Some(paths) => {
            if paths.len() > 4 {
                return Err("Attach at most four images per message.".into());
            }
            paths
                .into_iter()
                .map(|path| {
                    let raw = media::read_source(&path).map_err(|error| error.to_string())?;
                    admit(raw, crate::attachments::label_for(&path), Some(path))
                })
                .collect()
        }
        None => {
            let bytes = crate::attachments::clipboard_bytes().map_err(|error| error.to_string())?;
            Ok(vec![admit(bytes, "clipboard".into(), None)?])
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) fn restage(images: Vec<protocol::Image>) -> Result<Vec<Image>, String> {
    if images.len() > 4 {
        return Err("Too many images in the saved prompt".into());
    }
    images
        .into_iter()
        .map(|mut wire| {
            if wire.data.len() > 3_000_000 {
                return Err("The saved image exceeds the transport budget".into());
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&wire.data)
                .map_err(|_| "Invalid saved image".to_string())?;
            let image = media::normalize_for_transport(bytes, 2 * 1024 * 1024)
                .map_err(|error| error.to_string())?;
            if image.mime != wire.mime {
                return Err("The saved image MIME type does not match its bytes".into());
            }
            // The chip's dimensions and byte count describe exactly the bytes sent,
            // including an older saved image that needs normalization again.
            wire.data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
            wire.note = image.note.or(wire.note);
            Ok(Image {
                width: image.width,
                height: image.height,
                bytes: image.bytes.len(),
                source: None,
                wire,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            3,
            2,
            image::Rgba([3, 4, 5, 255]),
        ));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }

    fn staged(name: &str) -> Image {
        admit(png(), name.into(), None).unwrap()
    }

    #[test]
    fn retired_attachment_load_cannot_change_the_new_composer() {
        let mut pending = Pending::default();
        let old = pending.begin().unwrap();
        pending.reset();
        let current = pending.begin().unwrap();
        assert!(pending.accept(old, vec![staged("old")]).is_err());
        assert!(pending.is_loading());
        assert!(pending.accept(current, vec![staged("new")]).is_ok());
        assert_eq!(pending.wire()[0].name, "new");
    }

    #[test]
    fn loads_have_one_slot_and_four_images_and_failures_release_it() {
        let mut pending = Pending::default();
        let first = pending.begin().unwrap();
        assert!(pending.begin().is_err());
        pending.failed(first);
        let second = pending.begin().unwrap();
        assert!(
            pending
                .accept(second, (0..5).map(|_| staged("too many")).collect())
                .is_err()
        );
        assert!(pending.is_empty());
        let third = pending.begin().unwrap();
        pending
            .accept(third, (0..4).map(|_| staged("four")).collect())
            .unwrap();
        assert!(pending.begin().is_err());
    }

    #[tokio::test]
    async fn staged_image_survives_source_deletion_and_failed_batch_is_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("image.png");
        std::fs::write(&path, png()).unwrap();
        let admitted = load(&ImageSource::Paths(vec![path.clone()])).await.unwrap();
        let mut pending = Pending::default();
        let generation = pending.begin().unwrap();
        pending.accept(generation, admitted).unwrap();
        let before = pending.wire()[0].data.clone();
        std::fs::remove_file(&path).unwrap();
        assert!(load(&ImageSource::Paths(vec![path])).await.is_err());
        assert_eq!(pending.wire()[0].data, before);
        assert_eq!(pending.chips().len(), 1);
    }

    #[test]
    fn detach_uses_the_same_numbering_as_attachment_chips() {
        let mut pending = Pending::default();
        let generation = pending.begin().unwrap();
        pending
            .accept(generation, vec![staged("first"), staged("second")])
            .unwrap();
        assert!(pending.detach("2").contains("second"));
        assert_eq!(pending.wire()[0].name, "first");
        pending.detach("all");
        assert!(pending.is_empty());
    }

    #[test]
    fn saved_attachments_reject_corrupt_and_mismatched_payloads() {
        let mut wire = staged("saved").wire;
        wire.mime = "image/jpeg".into();
        assert!(restage(vec![wire.clone()]).is_err());
        wire.mime = "image/png".into();
        wire.data = "corrupt".into();
        assert!(restage(vec![wire]).is_err());
        let restored = restage(vec![staged("valid").wire]).unwrap();
        assert_eq!((restored[0].width, restored[0].height), (3, 2));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&restored[0].wire.data)
                .unwrap()
                .len(),
            restored[0].bytes
        );
    }
}
