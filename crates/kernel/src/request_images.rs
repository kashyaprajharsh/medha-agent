//! Which stored images a request carries, and in what form.

use crate::{ArtifactStore, CompiledContext, ContentPart, MediaSource, TextPart, TrustLabel};
use std::{collections::HashMap, sync::Arc};

/// Most images, and bytes of them, a request carries before old ones are retired.
const LIMIT: usize = 20;
const BUDGET: usize = 24_000_000;
/// Old images go a batch at a time, so the cached start of a chat moves rarely.
const BATCH: usize = 8;
const NEWEST_KEPT: usize = 3;
/// What a refused image is brought under, measured as sent: encoded, not on disk.
pub const SHRINK_TO: usize = 4 * 1024 * 1024;
/// The longest side a server takes when its refusal names none.
pub const SIDE: u32 = 8_000;
/// One image this small, in bytes as sent and in pixels, is not refused for size.
pub const SMALL: usize = 256 * 1024;
pub const SMALL_SIDE: u32 = 2_048;

/// An image made to fit: its bytes, media type and longest side.
pub type Fitted = (Vec<u8>, String, u32);

/// Makes an image fit a size on disk and a longest side, in pixels.
pub trait ImageShrinker: Send + Sync + 'static {
    fn shrink(&self, bytes: &[u8], budget: usize, max_side: u32) -> Result<Fitted, String>;
}

/// Size of `bytes` raw bytes once encoded for a request.
fn as_sent(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// The longest side a refusal says the server takes, else [`SIDE`]. Servers name
/// it in pixels, or as a count of 32-pixel patches for the whole image.
pub fn side_limit(refusal: &str) -> u32 {
    static NAMED: std::sync::LazyLock<[regex::Regex; 2]> = std::sync::LazyLock::new(|| {
        [
            r"max allowed size(?:\s+for [^:]+)?:\s*(\d{3,5})\s*pixels?",
            r"patches after processing.*exceeding the limit of\s*(\d{2,7})",
        ]
        .map(|pattern| regex::Regex::new(pattern).expect("side limit pattern"))
    });
    let said = refusal.to_ascii_lowercase();
    let number = |at: usize| -> Option<u32> { NAMED[at].captures(&said)?[1].parse().ok() };
    let side = number(0).or_else(|| number(1).map(|patches| patches.isqrt() * 32));
    side.filter(|side| (512..=SIDE).contains(side))
        .unwrap_or(SIDE)
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
    /// What to try once `sent` was refused this way, by a server taking `max_side`.
    pub fn after_refusal(self, sent: Sent, can_shrink: bool, max_side: u32) -> Self {
        let oversized = sent.largest > SHRINK_TO || sent.widest.is_none_or(|side| side > max_side);
        match self {
            Self::Whole if can_shrink && oversized => Self::Shrunk,
            Self::Whole | Self::Shrunk if !sent.one_small() => Self::OneSmall,
            _ => Self::AsText,
        }
    }
}

/// What a request ended up carrying.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub images: usize,
    /// Largest image, in bytes as sent.
    pub largest: usize,
    /// Longest side among them; `None` when any image's size in pixels is unknown.
    pub widest: Option<u32>,
    pub retired: usize,
    pub unreadable: usize,
}

impl Sent {
    /// One image, small in bytes and in pixels: nothing about size is left to rule out.
    pub fn one_small(&self) -> bool {
        self.images == 1
            && self.largest <= SMALL
            && self.widest.is_some_and(|side| side <= SMALL_SIDE)
    }
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
    max_side: u32,
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
    // Bytes as sent, and the longest side, each image is brought within.
    let fit = match sending {
        Sending::Whole | Sending::AsText => None,
        Sending::Shrunk => Some((SHRINK_TO, max_side)),
        Sending::OneSmall => Some((SMALL, max_side.min(SMALL_SIDE))),
    };
    let note = |text: &str| {
        ContentPart::Text(TextPart {
            text: text.to_owned(),
            provider_state: Vec::new(),
        })
    };
    let mut sent = Sent {
        widest: Some(0),
        ..Sent::default()
    };
    let mut encoded: HashMap<String, Option<(String, String, Option<u32>)>> = HashMap::new();
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
            let side = media.width.zip(media.height).map(|(w, h)| w.max(h));
            let image = match crate::artifacts::read_all(&store, &slot.hash).await {
                Ok(bytes) => fitted(bytes, media.mime_type.clone(), side, fit, shrinker).await,
                Err(_) => None,
            };
            let ready = image.map(|(bytes, kind, side)| {
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                (data, kind, side)
            });
            encoded.insert(slot.hash.clone(), ready);
        }
        match &encoded[&slot.hash] {
            Some((data, kind, side)) => {
                sent.images += 1;
                sent.largest = sent.largest.max(data.len());
                sent.widest = sent
                    .widest
                    .zip(*side)
                    .map(|(widest, side)| widest.max(side));
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

/// Bring an image within `fit`, bytes as sent and longest side; one that cannot
/// be goes as it is. A side not known is checked by the shrinker.
async fn fitted(
    bytes: Vec<u8>,
    kind: String,
    side: Option<u32>,
    fit: Option<(usize, u32)>,
    shrinker: Option<&Arc<dyn ImageShrinker>>,
) -> Option<(Vec<u8>, String, Option<u32>)> {
    let (Some((budget, max_side)), Some(shrinker)) = (fit, shrinker) else {
        return Some((bytes, kind, side));
    };
    if as_sent(bytes.len()) <= budget && side.is_some_and(|side| side <= max_side) {
        return Some((bytes, kind, side));
    }
    let shrinker = Arc::clone(shrinker);
    let on_disk = budget / 4 * 3;
    let work =
        tokio::task::spawn_blocking(move || (shrinker.shrink(&bytes, on_disk, max_side), bytes));
    match work.await.ok()? {
        (Ok((bytes, kind, side)), _) => Some((bytes, kind, Some(side))),
        (Err(_), bytes) => Some((bytes, kind, side)),
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
