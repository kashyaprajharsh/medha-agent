//! Keep cache-breaking pruning behind a savings gate and a regrowth threshold.

use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Default)]
pub(crate) struct PruneBatch(AtomicU32);

impl PruneBatch {
    pub(crate) fn ready(&self, message_tokens: u32) -> bool {
        message_tokens >= self.threshold()
    }

    pub(crate) fn threshold(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn restore(&self, threshold: u32) {
        self.0.store(threshold, Ordering::Relaxed);
    }

    pub(crate) fn accept(&self, before: u32, after: u32, usable: u32, trigger: u32) -> bool {
        let minimum = usable.div_ceil(20);
        let reclaimed = before.saturating_sub(after);
        if reclaimed == 0 || reclaimed < minimum {
            return false;
        }
        self.restore(after.saturating_add(reclaimed.max(trigger).max(minimum)));
        true
    }
}

#[cfg(test)]
#[path = "prune_batch_tests.rs"]
mod tests;
