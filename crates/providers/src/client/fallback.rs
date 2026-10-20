//! Stand-in models and saved keys to turn to when the chat's own cannot be used.

use super::{Connection, ProviderClient};
use crate::ProviderProfile;
use kernel::{Provider, ReasoningConfig};

/// Every key saved for the server at this address, first to last.
pub(super) type KeySource = Box<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// Looks up the models that stand in, in the user's order, when one is needed.
type StandIns = Box<dyn Fn() -> Vec<(ProviderProfile, String)> + Send + Sync>;

/// How long a limited model is rested when its server does not say.
const REST: std::time::Duration = std::time::Duration::from_secs(60);

/// The models to try, in the user's order, when their own cannot be used.
#[derive(Default)]
pub(super) struct Fallbacks {
    source: Option<StandIns>,
    /// While the user's own model is left alone after a limit.
    rested_until: Option<tokio::time::Instant>,
    listed: Vec<Connection>,
    next: usize,
    /// The user's own model and its reasoning setting, held while one stands in.
    own: Option<(Connection, ReasoningConfig)>,
}

impl Fallbacks {
    /// Read the list when the own model first fails this turn, not when the
    /// chat opened, leaving out the model that failed: it never stands in for itself.
    fn read(&mut self, client: &ProviderClient) {
        if self.own.is_some() || self.next != 0 {
            return;
        }
        let failed = client.connection.lock().unwrap().profile.clone();
        let listed = self.source.as_ref().map(|source| source());
        self.listed = listed
            .into_iter()
            .flatten()
            .filter(|(profile, _)| {
                profile.model != failed.model || profile.base_url != failed.base_url
            })
            .map(|(profile, credential)| Connection {
                profile,
                credential,
            })
            .collect();
    }
}

impl ProviderClient {
    /// Asked on each use, so a key saved while a chat is open joins the rotation.
    pub fn with_keys(self, source: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
        *self.keys.lock().unwrap() = Some(Box::new(source));
        self
    }

    /// The keys saved for the server in use now, in the order they are tried.
    fn saved_keys(&self) -> Vec<String> {
        let server = self.connection.lock().unwrap().profile.base_url.clone();
        let keys = self.keys.lock().unwrap();
        keys.as_ref()
            .map_or_else(Vec::new, |source| source(&server))
    }

    /// Asked when the model fails, so the list as saved at that moment is the one used.
    pub fn with_fallbacks(
        self,
        source: impl Fn() -> Vec<(ProviderProfile, String)> + Send + Sync + 'static,
    ) -> Self {
        self.fallbacks.lock().unwrap().source = Some(Box::new(source));
        self
    }

    pub(super) fn stand_in_for(
        &self,
        failed: &str,
        rest: kernel::recovery::Rest,
    ) -> Option<(String, String)> {
        let mut fallbacks = self.fallbacks.lock().unwrap();
        let was = self.active_model();
        // Another caller already left the model that failed: the one standing
        // in has not failed, and is not passed over for the next.
        if fallbacks.own.is_some() && !failed.is_empty() && failed != was {
            return Some((failed.to_owned(), was));
        }
        fallbacks.read(self);
        // One that cannot be switched to is passed over for the next.
        while let Some(next) = fallbacks.listed.get(fallbacks.next).cloned() {
            fallbacks.next += 1;
            let before = (self.connection.lock().unwrap().clone(), self.reasoning());
            if self
                .switch_provider_profile(next.profile, next.credential)
                .is_ok()
            {
                // Only the user's own model is rested: a stand-in that fails is passed.
                if let (None, kernel::recovery::Rest::Limited(wait)) = (&fallbacks.own, rest) {
                    let until = tokio::time::Instant::now() + wait.unwrap_or(REST);
                    fallbacks.rested_until = Some(until);
                }
                fallbacks.own.get_or_insert(before);
                return Some((was, self.active_model()));
            }
        }
        None
    }

    pub(super) fn stand_in_now(&self) -> Option<(String, String)> {
        let fallbacks = self.fallbacks.lock().unwrap();
        let (own, _) = fallbacks.own.as_ref()?;
        Some((own.profile.model.clone(), self.active_model()))
    }

    pub(super) fn has_stand_in_left(&self) -> bool {
        let mut fallbacks = self.fallbacks.lock().unwrap();
        fallbacks.read(self);
        fallbacks.next < fallbacks.listed.len()
    }

    pub(super) fn return_to_own(&self) -> Option<(String, String)> {
        let mut fallbacks = self.fallbacks.lock().unwrap();
        let resting = fallbacks
            .rested_until
            .is_some_and(|until| tokio::time::Instant::now() < until);
        if let (true, Some((own, _))) = (resting, &fallbacks.own) {
            return Some((own.profile.model.clone(), self.active_model()));
        }
        fallbacks.next = 0;
        fallbacks.rested_until = None;
        if let Some((own, reasoning)) = fallbacks.own.take() {
            *self.reasoning.lock().unwrap() = reasoning;
            *self.connection.lock().unwrap() = own;
        }
        drop(fallbacks);
        // Each message starts from the first saved key that is not resting.
        let keys = self.saved_keys();
        if let (true, Some((_, key))) = (keys.len() > 1, crate::key_rotation::first_ready(&keys)) {
            self.connection.lock().unwrap().credential = key.clone();
        }
        None
    }

    pub(super) fn has_another_key(&self) -> bool {
        if self.fallbacks.lock().unwrap().own.is_some() {
            return false;
        }
        let current = self.connection.lock().unwrap().credential.clone();
        let keys = self.saved_keys();
        keys.iter()
            .any(|key| *key != current && !crate::key_rotation::resting(key))
    }

    pub(super) fn turn_to_next_key(
        &self,
        status: u16,
        asked: Option<std::time::Duration>,
    ) -> Option<(usize, usize)> {
        // Only the chat's own model has keys of its own to turn to.
        if self.fallbacks.lock().unwrap().own.is_some() {
            return None;
        }
        let keys = self.saved_keys();
        if keys.len() < 2 {
            return None;
        }
        let current = self.connection.lock().unwrap().credential.clone();
        crate::key_rotation::rest_key(&current, crate::key_rotation::rest(status, asked));
        let (at, key) = crate::key_rotation::first_ready(&keys)?;
        self.connection.lock().unwrap().credential = key.clone();
        Some((at, keys.len()))
    }
}
