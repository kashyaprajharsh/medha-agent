//! Where plugins come from: local folders, git repositories pinned to a commit,
//! and marketplaces (repositories that list plugins).
//!
//! Fetching shells out to `git`, so private repositories use the operator's own
//! git credentials and nothing is stored by Medha.

use crate::{Error, io_error};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
}

impl GitSource {
    /// `owner` of a hosted `owner/repo` URL, used to namespace plugin ids.
    pub fn owner(&self) -> Option<String> {
        let path = self
            .url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .nth(1)?
            .to_string();
        Some(path).filter(|owner| !owner.is_empty())
    }

    /// Same repository and folder, whatever URL spelling or ref was used, so
    /// one plugin reached two ways is recognised as one.
    pub fn same_plugin(&self, other: &Self) -> bool {
        let repo = |source: &Self| {
            source
                .url
                .trim_end_matches('/')
                .trim_end_matches(".git")
                .to_ascii_lowercase()
        };
        let folder = |source: &Self| {
            source
                .subdir
                .as_deref()
                .map(|subdir| subdir.trim_matches('/').to_string())
                .filter(|subdir| !subdir.is_empty() && subdir != ".")
        };
        repo(self) == repo(other) && folder(self) == folder(other)
    }

    pub fn describe(&self) -> String {
        let mut text = self.url.clone();
        if let Some(subdir) = &self.subdir {
            text.push_str(&format!(" ({subdir})"));
        }
        if let Some(git_ref) = &self.git_ref {
            text.push_str(&format!(" @ {git_ref}"));
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    Local(PathBuf),
    Git(GitSource),
    /// `plugin@marketplace`.
    Listed {
        plugin: String,
        marketplace: String,
    },
}

/// Accepts a folder, `owner/repo[@ref]`, a GitHub or git URL (optionally with
/// `/tree/<ref>/<subdir>`), or `plugin@marketplace`.
pub fn parse(spec: &str) -> Spec {
    let spec = spec.trim();
    let path = Path::new(spec);
    if path.exists() || spec.starts_with(['.', '/', '~']) {
        return Spec::Local(path.to_path_buf());
    }
    let (body, git_ref) = split_ref(spec);
    if let Some(rest) = body
        .strip_prefix("https://github.com/")
        .or_else(|| body.strip_prefix("http://github.com/"))
        .or_else(|| body.strip_prefix("github.com/"))
    {
        return Spec::Git(github(rest, git_ref));
    }
    if body.contains("://") || body.starts_with("git@") {
        return Spec::Git(GitSource {
            url: body.to_string(),
            git_ref,
            subdir: None,
        });
    }
    if let Some((plugin, marketplace)) = spec.split_once('@')
        && !plugin.contains('/')
        && !marketplace.contains('/')
        && !plugin.is_empty()
        && !marketplace.is_empty()
    {
        return Spec::Listed {
            plugin: plugin.to_string(),
            marketplace: marketplace.to_string(),
        };
    }
    if body.split('/').count() == 2 && !body.split('/').any(str::is_empty) {
        return Spec::Git(github(body, git_ref));
    }
    Spec::Local(path.to_path_buf())
}

fn split_ref(spec: &str) -> (&str, Option<String>) {
    if let Some((body, git_ref)) = spec.rsplit_once('#') {
        return (body, Some(git_ref.to_string()));
    }
    match spec.rsplit_once('@') {
        Some((body, git_ref)) if body.contains('/') && !body.starts_with("git@") => {
            (body, Some(git_ref.to_string()))
        }
        _ => (spec, None),
    }
}

/// `owner/repo[/tree/<ref>/<subdir>]` on GitHub.
fn github(rest: &str, git_ref: Option<String>) -> GitSource {
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    let repo = parts
        .get(1)
        .copied()
        .unwrap_or_default()
        .trim_end_matches(".git");
    let mut source = GitSource {
        url: format!("https://github.com/{}/{repo}", parts[0]),
        git_ref,
        subdir: None,
    };
    if parts.get(2) == Some(&"tree") && parts.len() >= 4 {
        source.git_ref = source.git_ref.or_else(|| Some(parts[3].to_string()));
        if parts.len() > 4 {
            source.subdir = Some(parts[4..].join("/"));
        }
    }
    source
}

/// A fetched working tree, deleted when dropped.
pub struct Checkout {
    dir: PathBuf,
    pub commit: String,
}

impl Checkout {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The plugin folder inside the checkout.
    pub fn root(&self, source: &GitSource) -> Result<PathBuf, Error> {
        let root = match &source.subdir {
            Some(subdir) => self.dir.join(subdir.trim_start_matches("./")),
            None => self.dir.clone(),
        };
        if !root.starts_with(&self.dir) || root.components().any(|part| part.as_os_str() == "..") {
            return Err(Error::UnsafePackage(format!(
                "plugin folder {} is outside the repository",
                root.display()
            )));
        }
        Ok(root)
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Clones `source` at its ref (a branch, tag, or full commit) and records the
/// exact commit, which is what an approval and an update compare against.
pub fn fetch(source: &GitSource) -> Result<Checkout, Error> {
    let dir = std::env::temp_dir().join(format!("medha-plugin-{}", ulid::Ulid::new()));
    let mut checkout = Checkout {
        dir: dir.clone(),
        commit: String::new(),
    };
    let target = dir.to_string_lossy().into_owned();
    let is_commit = source.git_ref.as_deref().is_some_and(|git_ref| {
        git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit())
    });
    let mut clone = vec!["clone", "--quiet"];
    if !is_commit {
        clone.extend(["--depth", "1"]);
        if let Some(git_ref) = &source.git_ref {
            clone.extend(["--branch", git_ref]);
        }
    }
    clone.extend([source.url.as_str(), target.as_str()]);
    git(None, &clone)?;
    if let Some(commit) = source.git_ref.as_deref().filter(|_| is_commit) {
        git(Some(&dir), &["checkout", "--quiet", commit])?;
    }
    checkout.commit = git(Some(&dir), &["rev-parse", "HEAD"])?;
    resolve_links(&dir)?;
    Ok(checkout)
}

/// Packages never contain symlinks. In a fetched tree, a link to something
/// inside the repository is replaced by a copy; any other link is dropped.
fn resolve_links(dir: &Path) -> Result<(), Error> {
    let root = dir
        .canonicalize()
        .map_err(|error| io_error("reading checkout", dir, error))?;
    for _ in 0..8 {
        let links = links_under(&root);
        if links.is_empty() {
            return Ok(());
        }
        for link in links {
            let target = link.canonicalize().ok().filter(|target| {
                target.starts_with(&root) && !link.starts_with(target) && target != &root
            });
            // Windows removes directory links with `remove_dir`.
            std::fs::remove_file(&link)
                .or_else(|_| std::fs::remove_dir(&link))
                .map_err(|error| io_error("removing link", &link, error))?;
            if let Some(target) = target {
                copy_tree(&target, &link)?;
            }
        }
    }
    for link in links_under(&root) {
        let _ = std::fs::remove_file(&link).or_else(|_| std::fs::remove_dir(&link));
    }
    Ok(())
}

fn links_under(dir: &Path) -> Vec<PathBuf> {
    let mut links = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                links.push(entry.path());
            } else if kind.is_dir() && entry.file_name() != ".git" {
                pending.push(entry.path());
            }
        }
    }
    links
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let kind = std::fs::symlink_metadata(from)
        .map_err(|error| io_error("reading link target", from, error))?
        .file_type();
    if kind.is_symlink() {
        return Ok(());
    }
    if kind.is_file() {
        return std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|error| io_error("copying link target", from, error));
    }
    std::fs::create_dir_all(to).map_err(|error| io_error("copying link target", to, error))?;
    let entries =
        std::fs::read_dir(from).map_err(|error| io_error("copying link target", from, error))?;
    for entry in entries.filter_map(Result::ok) {
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, Error> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command
        .args(["-c", "advice.detachedHead=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| {
            Error::Source(format!(
                "git is needed to install from a repository and could not run: {error}"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Source(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The id a plugin installed as `plugin@marketplace` gets.
pub fn listed_id(marketplace: &str, plugin: &str) -> String {
    format!(
        "{}.{}",
        crate::compat::slug(marketplace),
        crate::compat::slug(plugin)
    )
}

/// A plugin a marketplace lists, as saved when the marketplace was added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub source: GitSource,
    /// Manifest fields the catalog supplies for the plugin, as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marketplace {
    pub name: String,
    pub source: GitSource,
    pub commit: String,
    #[serde(default)]
    pub plugins: Vec<Listing>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MarketplaceFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    seeded: Vec<String>,
    #[serde(default)]
    marketplaces: Vec<Marketplace>,
}

/// Saved marketplaces. Their catalogs are stored at add or refresh time, so
/// browsing needs no network.
#[derive(Debug, Clone)]
pub struct Marketplaces {
    path: PathBuf,
    defaults: Vec<GitSource>,
}

impl Marketplaces {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            defaults: Vec::new(),
        }
    }

    /// Catalogs added on first use. A default the user removes stays removed.
    pub fn with_defaults(mut self, defaults: Vec<GitSource>) -> Self {
        self.defaults = defaults;
        self
    }

    pub fn with_standard_defaults(self) -> Self {
        self.with_defaults(crate::compat::default_marketplaces())
    }

    pub fn pending_defaults(&self) -> Vec<GitSource> {
        let seeded = self.read().map(|file| file.seeded).unwrap_or_default();
        self.defaults
            .iter()
            .filter(|source| !seeded.contains(&source.url))
            .cloned()
            .collect()
    }

    /// Adds the pending defaults; one that cannot be fetched is retried next time.
    pub fn add_defaults(&self) -> Vec<Result<Marketplace, Error>> {
        self.pending_defaults()
            .into_iter()
            .map(|source| {
                let url = source.url.clone();
                let added = match self.list()?.into_iter().find(|m| m.source.url == url) {
                    Some(existing) => existing,
                    None => self.add(source)?,
                };
                let mut file = self.read()?;
                file.seeded.push(url);
                self.write(&file)?;
                Ok(added)
            })
            .collect()
    }

    pub fn list(&self) -> Result<Vec<Marketplace>, Error> {
        Ok(self.read()?.marketplaces)
    }

    pub fn get(&self, name: &str) -> Result<Marketplace, Error> {
        let find = || -> Result<Option<Marketplace>, Error> {
            Ok(self.list()?.into_iter().find(|market| market.name == name))
        };
        let mut found = find()?;
        if found.is_none() && !self.pending_defaults().is_empty() {
            self.add_defaults();
            found = find()?;
        }
        found.ok_or_else(|| {
            Error::Source(format!(
                "'{name}' is not an added marketplace. Add it with `/plugins marketplace \
                     add <owner/repo>`, or install straight from GitHub with \
                     `/plugins install <owner/repo>`"
            ))
        })
    }

    /// Fetches the repository's marketplace index and saves its catalog.
    pub fn add(&self, source: GitSource) -> Result<Marketplace, Error> {
        let checkout = fetch(&source)?;
        let (name, plugins) = crate::compat::read_marketplace(checkout.dir(), &source)?;
        let market = Marketplace {
            name,
            source,
            commit: checkout.commit.clone(),
            plugins,
        };
        let mut file = self.read()?;
        file.marketplaces
            .retain(|existing| existing.name != market.name);
        file.marketplaces.push(market.clone());
        file.marketplaces.sort_by(|a, b| a.name.cmp(&b.name));
        self.write(&file)?;
        Ok(market)
    }

    pub fn refresh(&self, name: &str) -> Result<Marketplace, Error> {
        let market = self.get(name)?;
        self.add(market.source)
    }

    pub fn remove(&self, name: &str) -> Result<(), Error> {
        let mut file = self.read()?;
        let before = file.marketplaces.len();
        file.marketplaces.retain(|market| market.name != name);
        if file.marketplaces.len() == before {
            return Err(Error::NotFound(format!("marketplace '{name}'")));
        }
        self.write(&file)
    }

    fn read(&self) -> Result<MarketplaceFile, Error> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| Error::State(format!("{}: {error}", self.path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(MarketplaceFile::default())
            }
            Err(error) => Err(io_error("reading marketplaces", &self.path, error)),
        }
    }

    fn write(&self, file: &MarketplaceFile) -> Result<(), Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_error("creating marketplace folder", parent, error))?;
        }
        let body = toml::to_string_pretty(file)
            .map_err(|error| Error::State(format!("serializing marketplaces: {error}")))?;
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, body)
            .map_err(|error| io_error("writing marketplaces", &temporary, error))?;
        std::fs::rename(&temporary, &self.path)
            .map_err(|error| io_error("saving marketplaces", &self.path, error))
    }
}

#[cfg(test)]
#[path = "sources_tests.rs"]
mod tests;
