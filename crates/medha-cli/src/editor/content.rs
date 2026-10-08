//! Validate every editor content block before admitting a prompt. Unsupported
//! content is an error, never silently discarded while answering its text.
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;

enum Source {
    Inline(String),
    File(PathBuf),
}

pub(super) async fn prompt(value: Value) -> Result<protocol::SendMessage, String> {
    let blocks = value
        .as_array()
        .ok_or("prompt must be a content-block array")?;
    if blocks.len() > 256 {
        return Err("At most 256 content blocks are supported".into());
    }
    let mut text = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => text.push(
                block["text"]
                    .as_str()
                    .ok_or("Text block has no text")?
                    .to_owned(),
            ),
            Some("image") => images.push(Source::Inline(
                block["data"]
                    .as_str()
                    .ok_or("Image has no data")?
                    .to_owned(),
            )),
            Some("resource") => {
                let resource = &block["resource"];
                if let Some(content) = resource["text"].as_str() {
                    text.push(content.to_owned());
                } else if resource["mimeType"]
                    .as_str()
                    .is_some_and(|mime| mime.starts_with("image/"))
                {
                    images.push(Source::Inline(
                        resource["blob"]
                            .as_str()
                            .ok_or("Image resource has no blob")?
                            .to_owned(),
                    ));
                } else {
                    return Err("Only text and image embedded resources are supported".into());
                }
            }
            Some("resource_link") => {
                let uri = block["uri"].as_str().ok_or("Resource link has no URI")?;
                let path = url::Url::parse(uri)
                    .ok()
                    .and_then(|url| url.to_file_path().ok());
                let image = block["mimeType"]
                    .as_str()
                    .is_some_and(|mime| mime.starts_with("image/"))
                    || path.as_ref().is_some_and(|path| {
                        crate::attachments::refs::has_image_extension(&path.to_string_lossy())
                    });
                if image {
                    images.push(Source::File(
                        path.ok_or("Image links must name a local file URI")?,
                    ));
                } else {
                    text.push(format!("[attached resource: {uri}]"));
                }
            }
            _ => return Err("Unsupported prompt content block".into()),
        }
    }
    if images.len() > crate::attachments::MAX_PER_MESSAGE {
        return Err("Attach at most four images".into());
    }
    let mut content = text.join("\n");
    if content.trim().is_empty() {
        if images.is_empty() {
            return Err("Prompt must contain text or an image".into());
        }
        content = crate::attachments::IMAGE_ONLY_PROMPT.into();
    }
    if images.is_empty() {
        return Ok(protocol::SendMessage {
            content,
            images: Vec::new(),
            intent: Some(protocol::SendIntent::Start),
        });
    }
    static DECODERS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let decoder = DECODERS
        .get_or_init(|| tokio::sync::Semaphore::new(2))
        .acquire()
        .await
        .map_err(|_| "Image preparation stopped")?;
    let images = tokio::task::spawn_blocking(move || {
        let _decoder = decoder;
        images
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                let (raw, name) = match source {
                    Source::Inline(data) => {
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(data.as_bytes())
                            .map_err(|_| "Invalid image base64")?;
                        (raw, format!("image {}", index + 1))
                    }
                    Source::File(path) => {
                        // Opening a FIFO must not strand an attachment worker.
                        let mut options = std::fs::OpenOptions::new();
                        options.read(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            options.custom_flags(libc::O_NONBLOCK);
                        }
                        let file = options.open(&path).map_err(|e| e.to_string())?;
                        let metadata = file.metadata().map_err(|e| e.to_string())?;
                        if !metadata.is_file() || metadata.len() > media::MAX_SOURCE_BYTES as u64 {
                            return Err(
                                "Image attachment must be a regular file within the source limit"
                                    .into(),
                            );
                        }
                        use std::io::Read;
                        let mut raw = Vec::new();
                        file.take(media::MAX_SOURCE_BYTES as u64 + 1)
                            .read_to_end(&mut raw)
                            .map_err(|e| e.to_string())?;
                        if raw.len() > media::MAX_SOURCE_BYTES {
                            return Err("Image attachment exceeds the source limit".into());
                        }
                        (raw, crate::attachments::label_for(&path))
                    }
                };
                let image = media::normalize_for_transport(raw, 2_200_000)
                    .map_err(|e| format!("image {}: {e}", index + 1))?;
                Ok(protocol::Image {
                    id: ulid::Ulid::new().to_string(),
                    name,
                    mime: image.mime.into(),
                    data: base64::engine::general_purpose::STANDARD.encode(image.bytes),
                    note: image.note,
                })
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .await
    .map_err(|e| format!("Attachment preparation stopped: {e}"))??;
    Ok(protocol::SendMessage {
        content,
        images,
        intent: Some(protocol::SendIntent::Start),
    })
}
