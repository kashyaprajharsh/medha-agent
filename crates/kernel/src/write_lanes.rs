//! Which changes may run at once: a writer waits only for writers it overlaps.

use std::sync::{Arc, Mutex};

/// Key of a change that must run alone in its workspace.
pub const WHOLE: &str = "state:*";

/// Key of a change to one file or folder tree.
pub fn path_key(path: &std::path::Path) -> String {
    format!("path:{}", path.display())
}

/// Whether two changes could touch the same state. An unknown key overlaps all.
fn overlap(left: &str, right: &str) -> bool {
    match (left.strip_prefix("path:"), right.strip_prefix("path:")) {
        (Some(left), Some(right)) => {
            let (left, right) = (std::path::Path::new(left), std::path::Path::new(right));
            left.starts_with(right) || right.starts_with(left)
        }
        // Memory is its own state: it meets other memory, never a file.
        (Some(_), None) => !right.starts_with("memory:"),
        (None, Some(_)) => !left.starts_with("memory:"),
        (None, None) => true,
    }
}

#[derive(Default)]
pub struct WriteLanes {
    /// Everyone holding or waiting, in arrival order.
    line: Mutex<Vec<(u64, String)>>,
    next: std::sync::atomic::AtomicU64,
    moved: tokio::sync::Notify,
}

/// A place in line; dropping it lets the writers behind it go.
pub struct Held {
    lanes: Arc<WriteLanes>,
    id: u64,
}

impl WriteLanes {
    /// Wait until no earlier arrival overlaps `key`. Later arrivals that overlap
    /// wait behind this one, so a wide key is never starved by narrow ones.
    pub async fn enter(self: &Arc<Self>, key: &str) -> Held {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.line.lock().unwrap().push((id, key.to_owned()));
        let held = Held {
            lanes: Arc::clone(self),
            id,
        };
        loop {
            let moved = self.moved.notified();
            tokio::pin!(moved);
            moved.as_mut().enable();
            let clear = {
                let line = self.line.lock().unwrap();
                let ahead = line.iter().take_while(|entry| entry.0 != id);
                !ahead.into_iter().any(|entry| overlap(&entry.1, key))
            };
            if clear {
                return held;
            }
            moved.await;
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.lanes
            .line
            .lock()
            .unwrap()
            .retain(|entry| entry.0 != self.id);
        self.lanes.moved.notify_waiters();
    }
}

/// The lock against other processes, taken once and shared by this process's
/// writers: lanes order them here, and two leases of one file would deadlock.
#[derive(Default)]
pub struct SharedLeases {
    /// Per lock the log names: how many share it, and the lease.
    open: Mutex<std::collections::HashMap<String, (usize, crate::events::MutationLease)>>,
    opening: tokio::sync::Mutex<()>,
}

/// One writer's share; the lease is released when the last share is dropped.
pub struct LeaseShare {
    leases: Arc<SharedLeases>,
    scope: String,
}

impl SharedLeases {
    pub async fn share<L: crate::EventLog + ?Sized>(
        self: &Arc<Self>,
        log: &L,
        key: &str,
    ) -> Result<LeaseShare, crate::KernelError> {
        let scope = log.mutation_lease_scope(key);
        let join = || match self.open.lock().unwrap().get_mut(&scope) {
            Some(entry) => {
                entry.0 += 1;
                true
            }
            None => false,
        };
        if !join() {
            let _one_at_a_time = self.opening.lock().await;
            if !join() {
                let lease = log.acquire_mutation_lease(key).await?;
                self.open.lock().unwrap().insert(scope.clone(), (1, lease));
            }
        }
        Ok(LeaseShare {
            leases: Arc::clone(self),
            scope,
        })
    }
}

impl Drop for LeaseShare {
    fn drop(&mut self) {
        let mut open = self.leases.open.lock().unwrap();
        let last = open.get_mut(&self.scope).is_some_and(|entry| {
            entry.0 -= 1;
            entry.0 == 0
        });
        if last {
            open.remove(&self.scope);
        }
    }
}

#[cfg(test)]
#[path = "write_lanes_tests.rs"]
mod tests;
