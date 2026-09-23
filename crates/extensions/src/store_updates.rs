//! Installing from repositories and marketplaces, updating to a newer commit,
//! and rolling back to the previous one.

use super::{Scope, Store};
use crate::package::{Package, copy_package};
use crate::sources::{self, Checkout, GitSource, Marketplaces, Spec};
use crate::state::{Grant, Origin, Pin, read_state_file};
use crate::{Error, MANIFEST_FILE, io_error};
use std::fs;
use std::path::{Path, PathBuf};

/// A fetched newer version, not yet applied. Surfaces show what changes, then
/// call [`Store::apply_update`].
pub struct UpdatePlan {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    pub from_commit: String,
    pub to_commit: String,
    /// Access the current version holds and the new version requests.
    pub access_before: Grant,
    pub access_after: Result<Grant, String>,
    pub was_enabled: bool,
    package: Package,
    _checkout: Checkout,
}

impl UpdatePlan {
    pub fn up_to_date(&self) -> bool {
        self.from_commit == self.to_commit
    }

    pub fn access_changed(&self) -> bool {
        self.access_after.as_ref() != Ok(&self.access_before)
    }
}

impl Store {
    /// Installs from a folder, `owner/repo[@ref]`, a git URL, or
    /// `plugin@marketplace`. The result is disabled until enabled.
    pub fn install_from(&self, spec: &str, markets: &Marketplaces) -> Result<Package, Error> {
        match sources::parse(spec) {
            Spec::Local(path) => self.install(path),
            Spec::Git(source) => self.install_git(&source, None, None),
            Spec::Listed {
                plugin,
                marketplace,
            } => {
                let market = markets.get(&marketplace)?;
                let Some(listing) = market.plugins.iter().find(|listing| listing.name == plugin)
                else {
                    let names: Vec<&str> = market
                        .plugins
                        .iter()
                        .map(|listing| listing.name.as_str())
                        .collect();
                    return Err(Error::Source(format!(
                        "marketplace {marketplace} does not list {plugin}; it lists: {}",
                        names.join(", ")
                    )));
                };
                let id = sources::listed_id(&market.name, &plugin);
                let entry = crate::compat::entry_for(listing);
                self.install_git(&listing.source, Some(id), Some(entry))
            }
        }
    }

    fn install_git(
        &self,
        source: &GitSource,
        id: Option<String>,
        entry: Option<String>,
    ) -> Result<Package, Error> {
        if let Some(existing) = self.installed_from(source) {
            return Err(Error::Source(format!(
                "this plugin is already installed as {existing}; use `/plugins update {existing}` \
                 for a newer version"
            )));
        }
        let checkout = sources::fetch(source)?;
        let root = checkout.root(source)?;
        let package = self.load_fetched(&root, source, id.as_deref(), entry.as_deref())?;
        self.install_package(
            &package,
            Some(Origin {
                source: source.clone(),
                commit: checkout.commit.clone(),
                entry,
            }),
        )
    }

    /// Native manifests carry their own id; other formats are namespaced by the
    /// repository owner so two authors' `review` plugins cannot collide.
    fn load_fetched(
        &self,
        root: &Path,
        source: &GitSource,
        id: Option<&str>,
        entry: Option<&str>,
    ) -> Result<Package, Error> {
        if let Some(entry) = entry {
            crate::compat::apply_entry(root, entry)?;
        }
        if root.join(MANIFEST_FILE).is_file() {
            return Package::load(root, &self.medha_version);
        }
        let id = match id {
            Some(id) => id.to_string(),
            None => {
                let probe = Package::load_as(root, None, &self.medha_version)?;
                let owner = source.owner().unwrap_or_else(|| "remote".into());
                format!(
                    "{}.{}",
                    crate::compat::slug(&owner),
                    crate::compat::slug(&probe.manifest.name)
                )
            }
        };
        Package::load_as(root, Some(&id), &self.medha_version)
    }

    pub fn plan_update(&self, id: &str) -> Result<UpdatePlan, Error> {
        let pin = self.user_pin(id)?;
        let origin = pin.origin.clone().ok_or_else(|| {
            Error::NotManaged(format!(
                "{id} was installed from a local folder; install it again to update"
            ))
        })?;
        let current = self.inspect(id, Some(Scope::User))?;
        let checkout = sources::fetch(&origin.source)?;
        let root = checkout.root(&origin.source)?;
        let package =
            self.load_fetched(&root, &origin.source, Some(id), origin.entry.as_deref())?;
        if package.manifest.id != id {
            return Err(Error::Source(format!(
                "the repository now contains plugin {}, not {id}",
                package.manifest.id
            )));
        }
        Ok(UpdatePlan {
            id: id.to_string(),
            from_version: current.package.manifest.version.clone(),
            to_version: package.manifest.version.clone(),
            from_commit: origin.commit,
            to_commit: checkout.commit.clone(),
            access_before: current.requested_grant().unwrap_or_default(),
            access_after: Grant::for_request(&package.manifest.permissions)
                .map_err(|error| error.to_string()),
            was_enabled: pin.enabled,
            package,
            _checkout: checkout,
        })
    }

    /// Replaces the files, keeping the old ones for `rollback`. A plugin stays
    /// on only if it was on and its access did not change, or the new access
    /// is `approved`.
    pub fn apply_update(&self, plan: UpdatePlan, approved: bool) -> Result<Package, Error> {
        if plan.up_to_date() {
            return self
                .inspect(&plan.id, Some(Scope::User))
                .map(|plugin| plugin.package);
        }
        let current = self.user_root.join(&plan.id);
        let previous = self.previous_dir(&plan.id);
        let staging = self
            .user_root
            .join(format!(".update-{}", ulid::Ulid::new()));
        copy_package(&plan.package.root, &staging)?;
        let staged = match Package::load_as(&staging, Some(&plan.id), &self.medha_version) {
            Ok(staged) if staged.content_hash == plan.package.content_hash => staged,
            outcome => {
                let _ = fs::remove_dir_all(&staging);
                return Err(outcome.err().unwrap_or(Error::SourceChanged));
            }
        };
        let _ = fs::remove_dir_all(&previous);
        if let Some(parent) = previous.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("creating rollback folder", parent, error))?;
        }
        fs::rename(&current, &previous)
            .map_err(|error| io_error("keeping the previous version", &current, error))?;
        if let Err(error) = fs::rename(&staging, &current) {
            let _ = fs::rename(&previous, &current);
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error("publishing the update", &current, error));
        }
        let requested = plan.access_after.clone().ok();
        let keep_on =
            plan.was_enabled && requested.is_some() && (approved || !plan.access_changed());
        let origin = self.user_pin(&plan.id)?.origin.map(|origin| Origin {
            commit: plan.to_commit.clone(),
            ..origin
        });
        self.update_state(Scope::User, |state| {
            let old = state
                .plugins
                .remove(&plan.id)
                .unwrap_or_else(|| Pin::new(Scope::User, String::new()));
            let mut pin = Pin::new(Scope::User, staged.content_hash.clone());
            pin.installed = true;
            pin.origin = origin;
            pin.enabled = keep_on;
            pin.grant = if keep_on {
                requested.unwrap_or_default()
            } else {
                Grant::default()
            };
            pin.previous = Some(Box::new(Pin {
                previous: None,
                ..old
            }));
            state.plugins.insert(plan.id.clone(), pin);
            Ok(())
        })?;
        Ok(staged)
    }

    /// Swaps back to the files and approval kept by the last update.
    pub fn rollback(&self, id: &str) -> Result<Package, Error> {
        let pin = self.user_pin(id)?;
        let backup = pin.previous.clone().ok_or_else(|| {
            Error::NotFound(format!("an earlier version of {id} to roll back to"))
        })?;
        let current = self.user_root.join(id);
        let previous = self.previous_dir(id);
        if !previous.is_dir() {
            return Err(Error::NotFound(format!("the kept files of {id}")));
        }
        let swap = self
            .user_root
            .join(format!(".rollback-{}", ulid::Ulid::new()));
        fs::rename(&current, &swap)
            .map_err(|error| io_error("moving the current version", &current, error))?;
        if let Err(error) = fs::rename(&previous, &current) {
            let _ = fs::rename(&swap, &current);
            return Err(io_error("restoring the previous version", &previous, error));
        }
        fs::rename(&swap, &previous)
            .map_err(|error| io_error("keeping the newer version", &swap, error))?;
        self.update_state(Scope::User, |state| {
            let mut restored = *backup;
            restored.previous = Some(Box::new(Pin {
                previous: None,
                ..pin
            }));
            state.plugins.insert(id.to_string(), restored);
            Ok(())
        })?;
        Package::load_as(&current, Some(id), &self.medha_version)
    }

    /// The id of an installed plugin fetched from the same repository folder.
    pub fn installed_from(&self, source: &GitSource) -> Option<String> {
        read_state_file(&self.user_state)
            .ok()?
            .plugins
            .into_iter()
            .find(|(_, pin)| {
                pin.installed
                    && pin
                        .origin
                        .as_ref()
                        .is_some_and(|origin| origin.source.same_plugin(source))
            })
            .map(|(id, _)| id)
    }

    /// Where an installed plugin was fetched from; `None` for local folders.
    pub fn origin(&self, id: &str) -> Option<Origin> {
        self.user_pin(id).ok()?.origin
    }

    pub fn can_rollback(&self, id: &str) -> bool {
        self.user_pin(id).is_ok_and(|pin| pin.previous.is_some()) && self.previous_dir(id).is_dir()
    }

    pub(crate) fn previous_dir(&self, id: &str) -> PathBuf {
        self.user_root.join(".previous").join(id)
    }

    fn user_pin(&self, id: &str) -> Result<Pin, Error> {
        read_state_file(&self.user_state)?
            .plugins
            .get(id)
            .filter(|pin| pin.installed)
            .cloned()
            .ok_or_else(|| Error::NotManaged(format!("{id} is not an installed user plugin")))
    }
}
