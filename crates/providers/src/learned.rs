//! What a server showed about a model, kept between sessions: a deployment can
//! differ from the catalogue (a vision model served with images turned off).

use kernel::ImageSupport;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A deployment can be reconfigured, so an answer is asked for again after this.
const TRUSTED_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    /// Model identity → whether it takes images, and when that was seen.
    #[serde(default)]
    images: HashMap<String, (bool, u64)>,
}

#[derive(Default)]
pub(crate) struct Learned {
    path: Option<PathBuf>,
    seen: Mutex<Stored>,
}

impl Learned {
    /// Remember in `path`, starting from what it already holds.
    pub(crate) fn at(path: PathBuf) -> Self {
        let seen = read(&path);
        Self {
            path: Some(path),
            seen: Mutex::new(seen),
        }
    }

    pub(crate) fn images(&self, identity: &str) -> Option<ImageSupport> {
        let seen = self.seen.lock().ok()?;
        let (takes, at) = *seen.images.get(identity)?;
        (now().saturating_sub(at) < TRUSTED_FOR.as_secs()).then_some(if takes {
            ImageSupport::Supported
        } else {
            ImageSupport::Unsupported
        })
    }

    pub(crate) fn set_images(&self, identity: &str, support: ImageSupport) {
        let takes = match support {
            ImageSupport::Supported => true,
            ImageSupport::Unsupported => false,
            ImageSupport::Unknown => return,
        };
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        if let Some(path) = &self.path {
            // Another session may have learned about other models meanwhile.
            let mut merged = read(path);
            merged.images.extend(std::mem::take(&mut seen.images));
            *seen = merged;
        }
        seen.images.insert(identity.to_owned(), (takes, now()));
        if let Some(path) = &self.path
            && let Ok(json) = serde_json::to_vec_pretty(&*seen)
        {
            let scratch = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&scratch, json).is_ok() && std::fs::rename(&scratch, path).is_err() {
                let _ = std::fs::remove_file(&scratch);
            }
        }
    }
}

fn read(path: &std::path::Path) -> Stored {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
#[path = "learned_tests.rs"]
mod tests;
