//! MEDHA memory layer (Phase 3 design doc, D1). Entries are events; the
//! projection here is a rebuildable cache over them. `MemoryStore` is the
//! public seam so the substrate stays swappable (P8) — recall/tools/CLI code
//! against the trait, not `MemoryProjection` directly.

pub mod entry;
pub mod projection;
pub mod recall;

pub use entry::{ConfidenceRung, MemoryEntry, MemoryKind, Scope};
pub use projection::{MemoryError, MemoryOp, MemoryProjection};

use async_trait::async_trait;

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError>;
    async fn update(&self, entry: MemoryEntry) -> Result<(), MemoryError>;
    async fn forget(&self, scope: Scope, name: &str) -> Result<(), MemoryError>;
    async fn pin(&self, scope: Scope, name: &str, pinned: bool) -> Result<(), MemoryError>;
    async fn get(&self, scope: Scope, name: &str) -> Result<Option<MemoryEntry>, MemoryError>;
    async fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError>;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError>;
}

#[async_trait]
impl MemoryStore for MemoryProjection {
    async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.apply(&MemoryOp::Write { entry })
    }

    async fn update(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.apply(&MemoryOp::Update { entry })
    }

    async fn forget(&self, scope: Scope, name: &str) -> Result<(), MemoryError> {
        self.apply(&MemoryOp::Forget { scope, name: name.to_string() })
    }

    async fn pin(&self, scope: Scope, name: &str, pinned: bool) -> Result<(), MemoryError> {
        self.apply(&MemoryOp::Pin { scope, name: name.to_string(), pinned })
    }

    async fn get(&self, scope: Scope, name: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        MemoryProjection::get(self, scope, name)
    }

    async fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        MemoryProjection::list(self)
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        MemoryProjection::search(self, query, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::TrustLabel;
    use ulid::Ulid;

    fn temp_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("medha-memory-lib-{tag}-{}", Ulid::new()));
        (dir.join("project.db"), dir.join("user.db"))
    }

    #[tokio::test]
    async fn store_trait_drives_the_projection() {
        let (p, u) = temp_paths("trait");
        let store: Box<dyn MemoryStore> = Box::new(MemoryProjection::open(&p, &u).unwrap());

        let e = MemoryEntry {
            name: "e1".into(),
            claim: "claim".into(),
            description: "hook".into(),
            kind: MemoryKind::Preference,
            scope: Scope::Project,
            trust: TrustLabel::User,
            confidence: ConfidenceRung::Candidate,
            provenance: vec![],
            sessions: vec![],
            version: 1,
            pinned: false,
            links: vec![],
            created: 0.0,
            updated: 0.0,
        };
        store.write(e.clone()).await.unwrap();
        assert_eq!(store.get(Scope::Project, "e1").await.unwrap().unwrap().claim, "claim");

        store.pin(Scope::Project, "e1", true).await.unwrap();
        assert!(store.get(Scope::Project, "e1").await.unwrap().unwrap().pinned);

        store.forget(Scope::Project, "e1").await.unwrap();
        assert!(store.get(Scope::Project, "e1").await.unwrap().is_none());

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
