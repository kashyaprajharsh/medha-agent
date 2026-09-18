//! Images staged on the composer for the next message.

use crate::attachments::{Attachment, MAX_PER_MESSAGE};

#[derive(Default)]
pub(super) struct PendingImages {
    staged: Vec<Attachment>,
    loading: bool,
    /// Bumped when the staged set is abandoned or sent, so a load still in
    /// flight cannot land in a session or turn that no longer expects it.
    generation: u64,
}

impl PendingImages {
    pub(super) fn is_loading(&self) -> bool {
        self.loading
    }

    pub(super) fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }

    /// Whether this path is already staged, so text still naming it in the
    /// composer does not attach it a second time.
    pub(super) fn holds(&self, path: &std::path::Path) -> bool {
        self.staged
            .iter()
            .any(|image| image.source.as_deref() == Some(path))
    }

    pub(super) fn begin(&mut self) -> Result<u64, String> {
        if self.loading {
            return Err("an image is still loading — wait for it to finish".into());
        }
        if self.staged.len() >= MAX_PER_MESSAGE {
            return Err(format!(
                "{MAX_PER_MESSAGE} images are already attached — /detach one first"
            ));
        }
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        Ok(self.generation)
    }

    /// Land a finished load. `None` means the result belongs to a set that has
    /// since been abandoned; it is dropped without touching the composer.
    pub(super) fn accept(
        &mut self,
        generation: u64,
        result: Result<Vec<Attachment>, String>,
    ) -> Option<String> {
        if generation != self.generation {
            return None;
        }
        self.loading = false;
        let images = match result {
            Ok(images) => images,
            Err(error) => return Some(format!("image attachment failed: {error}")),
        };
        if self.staged.len() + images.len() > MAX_PER_MESSAGE {
            return Some(format!(
                "attach at most {MAX_PER_MESSAGE} images per message"
            ));
        }
        let notice = images
            .iter()
            .map(|image| match &image.note {
                Some(note) => format!("attached {} ({note})", image.summary()),
                None => format!("attached {}", image.summary()),
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.staged.extend(images);
        Some(format!(
            "{notice}\ntype your message and press Enter, or /detach"
        ))
    }

    /// `/detach [number|all]`; the number is the one shown on the composer.
    pub(super) fn detach(&mut self, arg: &str) -> String {
        let arg = arg.trim();
        if arg.is_empty() || arg == "all" {
            self.reset();
            return "attachments cleared".into();
        }
        match arg.parse::<usize>() {
            Ok(index) if (1..=self.staged.len()).contains(&index) => {
                let removed = self.staged.remove(index - 1);
                format!("detached {} ({} staged)", removed.label, self.staged.len())
            }
            Ok(_) => "no attachment at that number".into(),
            Err(_) => "usage: /detach [number|all]".into(),
        }
    }

    /// Abandon the staged set and any load in flight. A session boundary
    /// (`/clear`, resume, rewind) must not carry images into another session.
    pub(super) fn reset(&mut self) {
        self.staged.clear();
        self.loading = false;
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn take(&mut self) -> Vec<kernel::MediaPart> {
        self.generation = self.generation.wrapping_add(1);
        std::mem::take(&mut self.staged)
            .into_iter()
            .map(|image| image.part)
            .collect()
    }

    /// Composer title, numbered so `/detach N` addresses what is on screen.
    pub(super) fn title(&self) -> Option<String> {
        if self.staged.is_empty() {
            return self.loading.then(|| " attaching image… ".to_string());
        }
        let list = self
            .staged
            .iter()
            .enumerate()
            .map(|(index, image)| format!("{}. {}", index + 1, image.summary()))
            .collect::<Vec<_>>()
            .join("  ");
        let tail = if self.loading { " · attaching…" } else { "" };
        Some(format!(" {list} · /detach [number|all]{tail} "))
    }

    /// Transcript label for a submitted message, so the scrollback records that
    /// the turn carried images rather than text alone.
    pub(super) fn submission_label(&self, line: &str) -> String {
        if self.staged.is_empty() {
            return line.to_string();
        }
        let list = self
            .staged
            .iter()
            .map(|image| image.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{line}\n[attached: {list}]")
    }
}

#[cfg(test)]
#[path = "attach_tests.rs"]
mod tests;
