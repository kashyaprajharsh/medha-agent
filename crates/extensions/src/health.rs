use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentState {
    Starting,
    Ready,
    Failed,
    Restarting,
}

impl ComponentState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Restarting => "restarting",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComponentHealth {
    pub component_id: String,
    pub state: ComponentState,
    pub last_failure: Option<String>,
    package_hash: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Health(Arc<RwLock<HashMap<(String, String), ComponentHealth>>>);

impl Health {
    pub(crate) fn begin(&self, plugin: &str, component: &str, hash: &str) {
        if let Ok(mut entries) = self.0.write() {
            let key = (plugin.to_string(), component.to_string());
            let previous = entries.get(&key).filter(|entry| entry.package_hash == hash);
            let restarting = previous.is_some_and(|entry| entry.state == ComponentState::Failed);
            let last_failure = previous.and_then(|entry| entry.last_failure.clone());
            entries.insert(
                key,
                ComponentHealth {
                    component_id: component.into(),
                    state: if restarting {
                        ComponentState::Restarting
                    } else {
                        ComponentState::Starting
                    },
                    last_failure,
                    package_hash: hash.into(),
                },
            );
        }
    }

    pub(crate) fn finish(&self, plugin: &str, component: &str, hash: &str, failure: Option<&str>) {
        if let Ok(mut entries) = self.0.write() {
            let Some(entry) = entries.get_mut(&(plugin.to_string(), component.to_string())) else {
                return;
            };
            if entry.package_hash != hash {
                return;
            }
            entry.state = if failure.is_some() {
                ComponentState::Failed
            } else {
                ComponentState::Ready
            };
            entry.last_failure = failure.map(str::to_string);
        }
    }

    pub(crate) fn for_plugin(&self, plugin: &str, hash: &str) -> Vec<ComponentHealth> {
        self.0.read().map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .iter()
                    .filter(|((id, _), health)| id == plugin && health.package_hash == hash)
                    .map(|(_, health)| health.clone())
                    .collect()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ComponentState, Health};

    #[test]
    fn failure_restarts_without_leaking_health_to_a_new_package_hash() {
        let health = Health::default();
        health.begin("dev.example.plugin", "hook", "old");
        health.finish("dev.example.plugin", "hook", "old", Some("hook failed"));
        health.begin("dev.example.plugin", "hook", "old");
        assert_eq!(
            health.for_plugin("dev.example.plugin", "old")[0].state,
            ComponentState::Restarting
        );
        health.finish("dev.example.plugin", "hook", "old", None);
        assert_eq!(
            health.for_plugin("dev.example.plugin", "old")[0].state,
            ComponentState::Ready
        );
        assert!(health.for_plugin("dev.example.plugin", "new").is_empty());
    }
}
