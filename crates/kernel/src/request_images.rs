//! Which stored images a request carries, and in what form.

use crate::{ArtifactStore, CompiledContext, ContentPart, MediaSource, TextPart, TrustLabel};
use std::{collections::HashMap, sync::Arc};

/// Most images, and bytes of them, a request carries before old ones are retired.
const LIMIT: usize = 20;
const BUDGET: usize = 24_000_000;
/// Old images go a batch at a time, so the cached start of a chat moves rarely.
const BATCH: usize = 8;
const NEWEST_KEPT: usize = 3;
/// What an image is brought under when a server finds a request too large.
pub const SHRINK_TO: usize = 4 * 1024 * 1024;
/// Small enough that a refusal cannot be about size.
pub const SMALL: usize = 256 * 1024;

/// Makes an image fit a byte budget, returning its bytes and media type.
pub trait ImageShrinker: Send + Sync + 'static {
    fn shrink(&self, bytes: &[u8], budget: usize) -> Result<(Vec<u8>, String), String>;
}

/// How a request sends its images; each is tried after the one before was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sending {
    Whole,
    Shrunk,
    OneSmall,
    /// Every image replaced by text; the caller does that, not `resolve_media`.
    AsText,
}

impl Sending {
    /// What to try once `sent` was refused this way.
    pub fn after_refusal(self, sent: Sent, can_shrink: bool) -> Self {
        let one_small = sent.images == 1 && sent.largest <= SMALL;
        match self {
            Self::Whole if can_shrink && sent.largest > SHRINK_TO => Self::Shrunk,
            Self::Whole | Self::Shrunk if !one_small => Self::OneSmall,
            _ => Self::AsText,
        }
    }
}

/// What a request ended up carrying.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub images: usize,
    pub largest: usize,
    pub retired: usize,
    pub unreadable: usize,
}

struct Slot {
    message: usize,
    part: usize,
    hash: String,
    size: usize,
    tool_made: bool,
}

/// Put image bytes into a disposable copy of the request; stored messages keep
/// their hashes. An image that is retired or unreadable becomes a note.
pub async fn resolve_media(
    context: &mut CompiledContext,
    store: Arc<dyn ArtifactStore>,
    sending: Sending,
    shrinker: Option<&Arc<dyn ImageShrinker>>,
) -> Sent {
    use base64::Engine;
    let mut messages = context.ordered_messages();
    let mut slots = Vec::new();
    for (message, entry) in messages.iter().enumerate() {
        // A tool's images ride in a message marked with where they came from.
        let tool_made = entry.trust.is_some_and(|trust| trust != TrustLabel::User);
        for (part, content) in entry.parts.iter().enumerate() {
            let ContentPart::Media(media) = content else {
                continue;
            };
            let MediaSource::Artifact(hash) = &media.source else {
                continue;
            };
            let size = match media.byte_size {
                Some(size) => size,
                None => Arc::clone(&store)
                    .size_async(hash.clone())
                    .await
                    .unwrap_or(0),
            };
            slots.push(Slot {
                message,
                part,
                hash: hash.clone(),
                size,
                tool_made,
            });
        }
    }
    let (retired, why) = match sending {
        Sending::OneSmall => (
            (0..slots.len()).map(|at| at + 1 < slots.len()).collect(),
            "[image not sent: the server refused this request's images, so only the newest is sent]",
        ),
        _ => (
            retired(&slots),
            "[older image no longer sent, to keep the request within limits]",
        ),
    };
    let budget = match sending {
        Sending::Whole | Sending::AsText => None,
        Sending::Shrunk => Some(SHRINK_TO),
        Sending::OneSmall => Some(SMALL),
    };
    let note = |text: &str| {
        ContentPart::Text(TextPart {
            text: text.to_owned(),
            provider_state: Vec::new(),
        })
    };
    let mut sent = Sent::default();
    let mut encoded: HashMap<String, Option<(String, String)>> = HashMap::new();
    for (slot, retire) in slots.iter().zip(retired) {
        let part = &mut messages[slot.message].parts[slot.part];
        if retire {
            sent.retired += 1;
            *part = note(why);
            continue;
        }
        let ContentPart::Media(media) = &mut *part else {
            continue;
        };
        if !encoded.contains_key(&slot.hash) {
            let image = match crate::artifacts::read_all(&store, &slot.hash).await {
                Ok(bytes) => fitted(bytes, media.mime_type.clone(), budget, shrinker).await,
                Err(_) => None,
            };
            encoded.insert(
                slot.hash.clone(),
                image.map(|(bytes, kind)| {
                    sent.largest = sent.largest.max(bytes.len());
                    (
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                        kind,
                    )
                }),
            );
        }
        match &encoded[&slot.hash] {
            Some((data, kind)) => {
                sent.images += 1;
                media.source = MediaSource::Base64(data.clone());
                media.mime_type = kind.clone();
            }
            None => {
                sent.unreadable += 1;
                *part = note("[image not shown: it could not be read back]");
            }
        }
    }
    context.ordered = Some(messages);
    sent
}

/// Shrink an image over `budget`; one that cannot be shrunk goes as it is.
async fn fitted(
    bytes: Vec<u8>,
    kind: String,
    budget: Option<usize>,
    shrinker: Option<&Arc<dyn ImageShrinker>>,
) -> Option<(Vec<u8>, String)> {
    let (Some(budget), Some(shrinker)) = (budget, shrinker) else {
        return Some((bytes, kind));
    };
    if bytes.len() <= budget {
        return Some((bytes, kind));
    }
    let shrinker = Arc::clone(shrinker);
    let work = tokio::task::spawn_blocking(move || (shrinker.shrink(&bytes, budget), bytes));
    match work.await.ok()? {
        (Ok(shrunk), _) => Some(shrunk),
        (Err(_), bytes) => Some((bytes, kind)),
    }
}

/// Which images are left out. Only a tool's are, a whole result at a time,
/// oldest first; what a person attached always stays.
fn retired(slots: &[Slot]) -> Vec<bool> {
    // One entry per tool result, oldest first: its message, images and bytes.
    let mut results: Vec<(usize, usize, usize)> = Vec::new();
    let mut reserved = (0, 0);
    for slot in slots {
        match results.last_mut() {
            _ if !slot.tool_made => reserved = (reserved.0 + 1, reserved.1 + slot.size),
            Some(last) if last.0 == slot.message => {
                *last = (last.0, last.1 + 1, last.2 + slot.size)
            }
            _ => results.push((slot.message, 1, slot.size)),
        }
    }
    let newest_first: Vec<(usize, usize)> = results.iter().rev().map(|r| (r.1, r.2)).collect();
    let retire = retire_count(&newest_first, reserved);
    slots
        .iter()
        .map(|slot| {
            slot.tool_made
                && results[..retire]
                    .iter()
                    .any(|result| result.0 == slot.message)
        })
        .collect()
}

/// How many of the oldest tool results lose their images. It moves in steps, not
/// with every new image, so the same ones are left out for several turns.
fn retire_count(newest_first: &[(usize, usize)], reserved: (usize, usize)) -> usize {
    let total = newest_first.len();
    let kept = |kept: usize| {
        newest_first[..kept]
            .iter()
            .fold(reserved, |sum, result| (sum.0 + result.0, sum.1 + result.1))
    };
    let bytes_fit = |count: usize| kept(count).1 <= BUDGET;
    let fits = |count: usize| kept(count).0 <= LIMIT && bytes_fit(count);
    if fits(total) {
        return 0;
    }
    let floor = NEWEST_KEPT.min(total);
    // The newest stay when attached images alone are too many: retiring
    // them would not help. Bytes are a hard limit, so they never shelter.
    let most = if !fits(0) && bytes_fit(floor) {
        total - floor
    } else {
        total
    };
    let window = (0..=total).rev().find(|count| fits(*count)).unwrap_or(0);
    let step = BATCH.min(window.saturating_sub(floor)).max(1);
    let mut retire = 0;
    while retire < most {
        retire = (retire + step).min(most);
        if fits(total - retire) {
            break;
        }
    }
    retire
}

#[cfg(test)]
#[path = "request_images_tests.rs"]
mod tests;
