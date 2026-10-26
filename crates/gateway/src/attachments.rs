//! Files a person sends. Pictures go to the model as images; other files are saved in the
//! chat's folder under `.medha/inbox`, where the agent can read them without a path approval.

use crate::platform::{Attachment, Platform};
use anyhow::{Context, Result};
use base64::Engine;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const INBOX: &str = ".medha/inbox";
const KEPT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Larger than any platform delivers to a bot, so nothing real is refused.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

pub(crate) struct Received {
    pub images: Vec<protocol::Image>,
    /// One line per file, for the message text.
    pub notes: Vec<String>,
}

pub(crate) async fn receive(
    platform: &dyn Platform,
    folder: &Path,
    files: &[Attachment],
) -> Received {
    let mut received = Received {
        images: Vec::new(),
        notes: Vec::new(),
    };
    for file in files {
        let picture = file.mime.starts_with("image/")
            && received.images.len() < runtime::attachments::MAX_PER_MESSAGE;
        let limit = if picture {
            runtime::attachments::MAX_SOURCE_BYTES as u64
        } else {
            MAX_FILE_BYTES
        };
        if file.size.is_some_and(|size| size > limit) {
            received
                .notes
                .push(format!("[{} was too large to receive]", file.name));
            continue;
        }
        let outcome = match platform.fetch(file).await {
            Ok(bytes) if picture => image(bytes, &file.name)
                .await
                .map(|image| received.images.push(image)),
            Ok(bytes) => save(folder, &file.name, &bytes).map(|path| {
                received
                    .notes
                    .push(format!("[file saved at {}]", path.display()));
            }),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            received
                .notes
                .push(format!("[{} could not be received: {error:#}]", file.name));
        }
    }
    received
}

async fn image(bytes: Vec<u8>, name: &str) -> Result<protocol::Image> {
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        let image = media::normalize_for_transport(bytes, runtime::attachments::TRANSPORT_BYTES)?;
        Ok(protocol::Image {
            id: format!("{:x}", u128::from_le_bytes(random()?)),
            name,
            mime: image.mime.into(),
            data: base64::engine::general_purpose::STANDARD.encode(image.bytes),
            note: image.note,
        })
    })
    .await
    .context("preparing the picture stopped")?
}

/// Saves under a cleaned name, never outside the inbox, and clears files older than a week.
fn save(folder: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let inbox = folder.join(INBOX);
    std::fs::create_dir_all(&inbox).with_context(|| format!("creating {}", inbox.display()))?;
    clear_old(&inbox);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let relative = Path::new(INBOX).join(format!("{stamp}-{}", clean(name)));
    std::fs::write(folder.join(&relative), bytes)
        .with_context(|| format!("saving {}", relative.display()))?;
    Ok(relative)
}

fn clear_old(inbox: &Path) {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > KEPT));
        if old && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A name a sender chose cannot climb out of the folder, hide itself or carry odd characters.
fn clean(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned.into()
    }
}

fn random() -> Result<[u8; 16]> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("no randomness: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sent_name_never_leaves_the_inbox_or_hides_itself() {
        assert_eq!(clean("../../.ssh/authorized_keys"), "authorized_keys");
        assert_eq!(clean("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(clean(".env"), "env");
        assert_eq!(clean(".."), "file");
        assert_eq!(clean("report final (2).pdf"), "report_final__2_.pdf");
        let folder = tempfile::tempdir().unwrap();
        let saved = save(folder.path(), "../../escape.txt", b"hi").unwrap();
        assert!(saved.starts_with(INBOX));
        assert_eq!(std::fs::read(folder.path().join(saved)).unwrap(), b"hi");
    }
}
