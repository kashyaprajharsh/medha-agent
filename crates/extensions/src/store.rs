use crate::package::{Package, PackageSource, copy_package};
use crate::state::{Grant, Pin, StateFile, parse_state_file, read_state_file, write_state_file};
use crate::{Error, io_error};
use medha_extension_api::ExtensionComponent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Disabled,
    Enabled,
    /// An enabled package changed after approval. It is never loaded until the
    /// operator explicitly enables the new hash.
    Changed,
    /// More than one package in one scope claims the same stable ID.
    Collision,
    /// A repository package reuses the ID of a user package. The user package
    /// keeps its state; a checkout can never disable or replace it.
    Shadowed,
}

impl Activation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Changed => "changed (disabled)",
            Self::Collision => "collision (disabled)",
            Self::Shadowed => "ignored (a user plugin has this id)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListedPlugin {
    pub package: Package,
    pub scope: Scope,
    pub activation: Activation,
    /// The access approved for this exact hash, when enabled.
    pub grant: Option<Grant>,
}

impl ListedPlugin {
    /// Access this package needs; an error explains why it can never be granted.
    pub fn requested_grant(&self) -> Result<Grant, Error> {
        Grant::for_request(&self.package.manifest.permissions)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryError {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub plugins: Vec<ListedPlugin>,
    pub errors: Vec<DiscoveryError>,
}

impl Discovery {
    pub fn enabled(&self) -> impl Iterator<Item = &ListedPlugin> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.activation == Activation::Enabled)
    }

    /// Operator-facing warnings for packages that are not active although the
    /// operator expects them to be, plus unreadable packages and state.
    pub fn notices(&self) -> Vec<String> {
        let mut notices: Vec<String> = self
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.error))
            .collect();
        for plugin in &self.plugins {
            let id = &plugin.package.manifest.id;
            let scope = plugin.scope.as_str();
            match plugin.activation {
                Activation::Changed => notices.push(format!(
                    "{id} ({scope}) changed after it was enabled and stays off; \
                     run `medha plugins enable {id}` to approve the new files"
                )),
                Activation::Collision => notices.push(format!(
                    "{id} ({scope}) is claimed by more than one package and stays off"
                )),
                Activation::Shadowed => notices.push(format!(
                    "{id} in this repository is ignored because a user plugin has the same id"
                )),
                Activation::Disabled | Activation::Enabled => {}
            }
        }
        notices
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorAction {
    pub id: String,
    pub plugin_id: String,
    pub title: String,
    pub description: String,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct Store {
    user_root: PathBuf,
    project_root: PathBuf,
    user_state: PathBuf,
    project_state: PathBuf,
    medha_version: String,
    hook_sources: Vec<HookSource>,
}

/// A folder of hooks that is not a plugin: a `hooks.toml` folder or a settings
/// folder read by the compat adapter. It is listed under a fixed id.
#[derive(Debug, Clone)]
pub struct HookSource {
    pub dir: PathBuf,
    pub scope: Scope,
    pub id: &'static str,
    pub kind: PackageSource,
}

impl HookSource {
    /// `<workspace>/.medha/hooks`, `~/.medha/hooks`, and the settings folders.
    pub fn standard(workspace: &Path, medha_home: &Path, home: Option<&Path>) -> Vec<Self> {
        let mut sources = vec![
            Self {
                dir: workspace.join(".medha").join("hooks"),
                scope: Scope::Project,
                id: crate::PROJECT_HOOKS_ID,
                kind: PackageSource::HookSet,
            },
            Self {
                dir: medha_home.join("hooks"),
                scope: Scope::User,
                id: crate::USER_HOOKS_ID,
                kind: PackageSource::HookSet,
            },
            Self {
                dir: workspace.join(crate::compat::SETTINGS_DIR),
                scope: Scope::Project,
                id: crate::compat::PROJECT_SETTINGS_HOOKS_ID,
                kind: PackageSource::Settings,
            },
        ];
        if let Some(home) = home {
            sources.push(Self {
                dir: home.join(crate::compat::SETTINGS_DIR),
                scope: Scope::User,
                id: crate::compat::USER_SETTINGS_HOOKS_ID,
                kind: PackageSource::Settings,
            });
        }
        sources
    }
}

impl Store {
    /// User pins live beside the user store so one approval holds in every
    /// workspace; project pins are per workspace because the package is.
    pub fn new(
        user_root: PathBuf,
        project_root: PathBuf,
        user_state: PathBuf,
        project_state: PathBuf,
        medha_version: impl Into<String>,
    ) -> Self {
        Self {
            user_root,
            project_root,
            user_state,
            project_state,
            medha_version: medha_version.into(),
            hook_sources: Vec::new(),
        }
    }

    pub fn with_hook_sources(mut self, sources: Vec<HookSource>) -> Self {
        self.hook_sources = sources;
        self
    }

    pub fn medha_version(&self) -> &str {
        &self.medha_version
    }

    /// A plugin's private writable folder: kept across updates and disable,
    /// deleted only with the plugin.
    pub fn data_dir(&self, id: &str) -> PathBuf {
        self.user_root
            .parent()
            .unwrap_or(&self.user_root)
            .join("plugin-data")
            .join(id)
    }

    pub fn discover(&self) -> Result<Discovery, Error> {
        let mut discovery = Discovery::default();
        let user_state = self.read_state_or_report(Scope::User, &mut discovery);
        let project_state = self.read_state_or_report(Scope::Project, &mut discovery);
        self.discover_root(&self.project_root, Scope::Project, &mut discovery);
        self.discover_root(&self.user_root, Scope::User, &mut discovery);
        self.discover_hook_sources(&mut discovery);

        let mut counts = HashMap::<(String, Scope), usize>::new();
        for plugin in &discovery.plugins {
            *counts
                .entry((plugin.package.manifest.id.clone(), plugin.scope))
                .or_default() += 1;
        }
        for plugin in &mut discovery.plugins {
            let id = plugin.package.manifest.id.clone();
            let state = match plugin.scope {
                Scope::User => &user_state,
                Scope::Project => &project_state,
            };
            if counts[&(id.clone(), plugin.scope)] > 1 {
                plugin.activation = Activation::Collision;
            } else if plugin.scope == Scope::Project && counts.contains_key(&(id, Scope::User)) {
                plugin.activation = Activation::Shadowed;
            } else {
                (plugin.activation, plugin.grant) = activation_for(state, plugin);
            }
        }
        discovery.plugins.sort_by(|left, right| {
            left.package
                .manifest
                .id
                .cmp(&right.package.manifest.id)
                .then_with(|| left.scope.cmp(&right.scope))
        });
        discovery
            .errors
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(discovery)
    }

    pub fn install(&self, source: impl AsRef<Path>) -> Result<Package, Error> {
        let package = Package::load(source, &self.medha_version)?;
        self.install_package(&package, None)
    }

    /// Copies a loaded package into the managed store, disabled. `origin`
    /// records where it was fetched from so it can be updated later.
    pub(crate) fn install_package(
        &self,
        package: &Package,
        origin: Option<crate::state::Origin>,
    ) -> Result<Package, Error> {
        let id = package.manifest.id.clone();
        if self
            .discover()?
            .plugins
            .iter()
            .any(|plugin| plugin.package.manifest.id == id)
        {
            return Err(Error::AlreadyExists(id));
        }
        fs::create_dir_all(&self.user_root)
            .map_err(|error| io_error("creating plugin directory", &self.user_root, error))?;
        let destination = self.user_root.join(&id);
        if destination.exists() {
            return Err(Error::AlreadyExists(id));
        }
        let staging = self.user_root.join(format!(
            ".install-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        copy_package(&package.root, &staging)?;
        let staged = match Package::load_as(&staging, Some(&id), &self.medha_version) {
            Ok(staged)
                if staged.manifest == package.manifest
                    && staged.content_hash == package.content_hash =>
            {
                staged
            }
            Ok(_) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(Error::SourceChanged);
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        // Publish a disabled pin before package code becomes discoverable. This
        // also closes the window where an old stale pin could activate a newly
        // installed package with the same ID.
        if let Err(error) = self.update_state(Scope::User, |state| {
            let mut pin = Pin::new(Scope::User, staged.content_hash.clone());
            pin.installed = true;
            pin.origin = origin;
            state.plugins.insert(id.clone(), pin);
            Ok(())
        }) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = fs::rename(&staging, &destination) {
            let _ = fs::remove_dir_all(&staging);
            let _ = self.forget_pin(Scope::User, &id);
            return Err(io_error("publishing plugin", &destination, error));
        }
        match Package::load_as(&destination, Some(&id), &self.medha_version) {
            Ok(installed)
                if installed.content_hash == staged.content_hash
                    && installed.manifest == staged.manifest =>
            {
                Ok(installed)
            }
            outcome => {
                let _ = fs::remove_dir_all(&destination);
                let _ = self.forget_pin(Scope::User, &id);
                Err(outcome.err().unwrap_or(Error::SourceChanged))
            }
        }
    }

    /// Operator actions are deliberately separate from model tools. Surfaces can
    /// build a command palette from this list without increasing provider schema.
    pub fn actions(&self) -> Result<Vec<OperatorAction>, Error> {
        let mut actions = Vec::new();
        for plugin in self
            .discover()?
            .plugins
            .into_iter()
            .filter(|plugin| plugin.activation == Activation::Enabled)
        {
            for component in &plugin.package.manifest.components {
                if let ExtensionComponent::Action {
                    id,
                    title,
                    description,
                    prompt,
                } = component
                {
                    actions.push(OperatorAction {
                        id: format!("{}/{}", plugin.package.manifest.id, id),
                        plugin_id: plugin.package.manifest.id.clone(),
                        title: title.clone(),
                        description: description.clone(),
                        prompt: prompt.clone(),
                    });
                }
            }
        }
        actions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(actions)
    }

    /// Enables the currently discovered hash. `approved` must equal the access
    /// that hash requests, so an operator always approves exactly what runs.
    pub fn enable(
        &self,
        id: &str,
        scope: Option<Scope>,
        approved: &Grant,
    ) -> Result<Package, Error> {
        let plugin = self.select(id, scope, false)?;
        let required = plugin.requested_grant()?;
        if &required != approved {
            return Err(Error::GrantRequired(format!(
                "{id} requests access that was not approved; review it with \
                 `medha plugins inspect {id}` and enable it with --grant"
            )));
        }
        let package = plugin.package.clone();
        self.update_state(plugin.scope, |state| {
            let pin = state
                .plugins
                .entry(id.to_string())
                .or_insert_with(|| Pin::new(plugin.scope, package.content_hash.clone()));
            pin.scope = plugin.scope;
            pin.content_hash.clone_from(&package.content_hash);
            pin.enabled = true;
            pin.grant = required;
            Ok(())
        })?;
        Ok(package)
    }

    pub fn disable(&self, id: &str, scope: Option<Scope>) -> Result<Package, Error> {
        let plugin = self.select(id, scope, true)?;
        let package = plugin.package.clone();
        self.update_state(plugin.scope, |state| {
            let pin = state
                .plugins
                .entry(id.to_string())
                .or_insert_with(|| Pin::new(plugin.scope, package.content_hash.clone()));
            pin.scope = plugin.scope;
            pin.content_hash.clone_from(&package.content_hash);
            pin.enabled = false;
            pin.grant = Grant::default();
            Ok(())
        })?;
        Ok(package)
    }

    pub fn remove(&self, id: &str) -> Result<(), Error> {
        let plugin = match self.select(id, Some(Scope::User), true) {
            Ok(plugin) => plugin,
            Err(Error::NotFound(_)) => {
                if let Ok(project) = self.select(id, Some(Scope::Project), true) {
                    return Err(Error::NotManaged(format!(
                        "{id} is part of this repository; delete {} to remove it",
                        project.package.root.display()
                    )));
                }
                return Err(Error::NotFound(id.to_string()));
            }
            Err(error) => return Err(error),
        };
        if plugin.package.source != PackageSource::Plugin {
            return Err(Error::NotManaged(format!(
                "{id} comes from {}; edit or delete that file to remove its hooks",
                plugin.package.root.display()
            )));
        }
        let installed = read_state_file(&self.user_state)?
            .plugins
            .get(id)
            .is_some_and(|pin| pin.installed);
        let expected = self.user_root.join(id);
        let expected = expected
            .canonicalize()
            .map_err(|error| io_error("resolving installed plugin", &expected, error))?;
        if !installed || plugin.package.root != expected {
            return Err(Error::NotManaged(format!(
                "{id} at {} was not installed by Medha; delete it yourself",
                plugin.package.root.display()
            )));
        }
        // Publish the disabled state before deleting code. If the state write
        // fails, the package remains intact and cannot be half-uninstalled.
        self.disable(id, Some(Scope::User))?;
        fs::remove_dir_all(&expected)
            .map_err(|error| io_error("removing plugin", &expected, error))?;
        let _ = fs::remove_dir_all(self.previous_dir(id));
        let _ = fs::remove_dir_all(self.data_dir(id));
        self.forget_pin(Scope::User, id)
    }

    /// Hook folders found in the workspace or home that the operator has never
    /// decided on, or that changed after approval. Installed plugins are
    /// excluded: installing them was already a decision.
    pub fn needs_review(&self) -> Result<Vec<ListedPlugin>, Error> {
        let discovery = self.discover()?;
        Ok(discovery
            .plugins
            .into_iter()
            .filter(|plugin| plugin.package.source != PackageSource::Plugin)
            .filter(|plugin| match plugin.activation {
                Activation::Changed => true,
                Activation::Disabled => parse_state_file(self.state_path(plugin.scope))
                    .map(|state| !state.plugins.contains_key(&plugin.package.manifest.id))
                    .unwrap_or(false),
                _ => false,
            })
            .collect())
    }

    pub fn inspect(&self, id: &str, scope: Option<Scope>) -> Result<ListedPlugin, Error> {
        self.select(id, scope, true)
    }

    /// Reloads one package and its pin without rescanning every package.
    pub(crate) fn recheck(
        &self,
        root: &Path,
        id: &str,
        source: &PackageSource,
        scope: Scope,
    ) -> Result<ListedPlugin, Error> {
        let package = crate::package::reload(root, id, source, &self.medha_version)?;
        let state = read_state_file(self.state_path(scope))?;
        let mut plugin = ListedPlugin {
            package,
            scope,
            activation: Activation::Disabled,
            grant: None,
        };
        (plugin.activation, plugin.grant) = activation_for(&state, &plugin);
        Ok(plugin)
    }

    fn select(
        &self,
        id: &str,
        scope: Option<Scope>,
        allow_inactive: bool,
    ) -> Result<ListedPlugin, Error> {
        let mut matches: Vec<ListedPlugin> = self
            .discover()?
            .plugins
            .into_iter()
            .filter(|plugin| {
                plugin.package.manifest.id == id && scope.is_none_or(|scope| plugin.scope == scope)
            })
            .collect();
        if matches.len() > 1 {
            matches.retain(|plugin| plugin.activation != Activation::Shadowed);
        }
        let plugin = match matches.len() {
            0 => return Err(Error::NotFound(id.to_string())),
            1 => matches.remove(0),
            _ => return Err(Error::Collision(id.to_string())),
        };
        match plugin.activation {
            Activation::Collision if !allow_inactive => Err(Error::Collision(id.to_string())),
            Activation::Shadowed if !allow_inactive => Err(Error::NotManaged(format!(
                "the repository copy of {id} is ignored because a user plugin has the same id"
            ))),
            _ => Ok(plugin),
        }
    }

    fn discover_root(&self, root: &Path, scope: Scope, out: &mut Discovery) {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                out.errors.push(DiscoveryError {
                    path: root.to_path_buf(),
                    error: error.to_string(),
                });
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                    {
                        paths.push(path);
                    }
                }
                Err(error) => out.errors.push(DiscoveryError {
                    path: root.to_path_buf(),
                    error: error.to_string(),
                }),
            }
        }
        paths.sort();
        for path in paths {
            match Package::load(&path, &self.medha_version) {
                Ok(package) => out.plugins.push(ListedPlugin {
                    package,
                    scope,
                    activation: Activation::Disabled,
                    grant: None,
                }),
                Err(error) => out.errors.push(DiscoveryError {
                    path,
                    error: error.to_string(),
                }),
            }
        }
    }

    fn discover_hook_sources(&self, out: &mut Discovery) {
        for source in &self.hook_sources {
            if !source.dir.is_dir() {
                continue;
            }
            match crate::package::reload(&source.dir, source.id, &source.kind, &self.medha_version)
            {
                Ok(package) => out.plugins.push(ListedPlugin {
                    package,
                    scope: source.scope,
                    activation: Activation::Disabled,
                    grant: None,
                }),
                Err(Error::NotFound(_)) => {}
                Err(error) => out.errors.push(DiscoveryError {
                    path: source.dir.clone(),
                    error: error.to_string(),
                }),
            }
        }
    }

    fn state_path(&self, scope: Scope) -> &Path {
        match scope {
            Scope::User => &self.user_state,
            Scope::Project => &self.project_state,
        }
    }

    /// Unreadable state disables that scope and is reported; it is never
    /// overwritten, because `update_state` rereads and refuses it.
    fn read_state_or_report(&self, scope: Scope, discovery: &mut Discovery) -> StateFile {
        let path = self.state_path(scope);
        parse_state_file(path).unwrap_or_else(|reason| {
            discovery.errors.push(DiscoveryError {
                path: path.to_path_buf(),
                error: format!(
                    "{reason}; {} plugins stay off until the file is repaired or moved aside",
                    scope.as_str()
                ),
            });
            StateFile::default()
        })
    }

    fn forget_pin(&self, scope: Scope, id: &str) -> Result<(), Error> {
        self.update_state(scope, |state| {
            state.plugins.remove(id);
            Ok(())
        })
    }

    fn update_state(
        &self,
        scope: Scope,
        operation: impl FnOnce(&mut StateFile) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let state_path = self.state_path(scope);
        let parent = state_path
            .parent()
            .ok_or_else(|| Error::State("plugin state path has no parent".into()))?;
        fs::create_dir_all(parent)
            .map_err(|error| io_error("creating plugin state directory", parent, error))?;
        let lock_path = state_path.with_extension("lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| io_error("opening plugin state lock", &lock_path, error))?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _guard = lock
            .write()
            .map_err(|error| Error::State(format!("locking plugin state: {error}")))?;
        let mut state = read_state_file(state_path)?;
        operation(&mut state)?;
        write_state_file(state_path, &state)
    }
}

fn activation_for(state: &StateFile, plugin: &ListedPlugin) -> (Activation, Option<Grant>) {
    let Some(pin) = state.plugins.get(&plugin.package.manifest.id) else {
        return (Activation::Disabled, None);
    };
    if !pin.enabled || pin.scope != plugin.scope {
        return (Activation::Disabled, None);
    }
    let still_granted = plugin
        .requested_grant()
        .is_ok_and(|required| required == pin.grant);
    if pin.content_hash == plugin.package.content_hash && still_granted {
        (Activation::Enabled, Some(pin.grant.clone()))
    } else {
        (Activation::Changed, None)
    }
}

#[path = "store_updates.rs"]
mod updates;
pub use updates::UpdatePlan;

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
