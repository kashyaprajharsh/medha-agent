use crate::{Error, MANIFEST_FILE, Manifest, io_error};
use medha_extension_api::ExtensionComponent;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_PACKAGE_FILES: usize = 512;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Package {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub content_hash: String,
    pub files: usize,
    pub bytes: u64,
    pub source: PackageSource,
}

/// How a package was read, so rechecks reload it the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Plugin,
    /// A `hooks.toml` folder; its hooks are packaged under a fixed id.
    HookSet,
    /// Hooks declared in another tool's settings files, read by `compat`.
    Settings,
}

impl Package {
    pub fn load(root: impl AsRef<Path>, medha_version: &str) -> Result<Self, Error> {
        Self::load_as(root, None, medha_version)
    }

    /// `id` names a package whose own format has no reverse-domain id, such as
    /// one fetched from `owner/repo`; it is ignored for native manifests.
    pub fn load_as(
        root: impl AsRef<Path>,
        id: Option<&str>,
        medha_version: &str,
    ) -> Result<Self, Error> {
        let root = real_dir(root.as_ref())?;
        let manifest_path = root.join(MANIFEST_FILE);
        if !manifest_path.is_file() && crate::compat::is_plugin(&root) {
            return crate::compat::load_plugin(&root, id, medha_version);
        }
        let manifest: Manifest = parse_toml(&manifest_path)?;
        let tree = root.clone();
        Self::assemble(
            root,
            Some(&tree),
            manifest,
            medha_version,
            PackageSource::Plugin,
            &[],
        )
    }

    pub fn reload(&self, medha_version: &str) -> Result<Self, Error> {
        reload(&self.root, &self.manifest.id, &self.source, medha_version)
    }

    /// Validates a manifest and hashes `tree` (none for settings-only hooks);
    /// `seed` is approved input that lives outside the tree, such as settings text.
    pub(crate) fn assemble(
        root: PathBuf,
        tree: Option<&Path>,
        manifest: Manifest,
        medha_version: &str,
        source: PackageSource,
        seed: &[u8],
    ) -> Result<Self, Error> {
        manifest
            .validate(medha_version)
            .map_err(|error| Error::Manifest(error.to_string()))?;
        let files = match tree {
            Some(tree) => scan_files(tree)?,
            None => Vec::new(),
        };
        validate_component_files(&root, &manifest)?;
        let mut hash = Sha256::new();
        hash.update((seed.len() as u64).to_be_bytes());
        hash.update(seed);
        let mut bytes = 0u64;
        for file in &files {
            let relative = file
                .strip_prefix(tree.unwrap_or(&root))
                .expect("scanner only returns package descendants");
            let relative = portable_relative_path(relative)?;
            let content = read_regular_bounded(file, MAX_FILE_BYTES)?;
            bytes = bytes
                .checked_add(content.len() as u64)
                .ok_or_else(|| Error::UnsafePackage("package byte count overflowed".into()))?;
            if bytes > MAX_PACKAGE_BYTES {
                return Err(Error::UnsafePackage(format!(
                    "package is larger than {} bytes",
                    MAX_PACKAGE_BYTES
                )));
            }
            hash.update((relative.len() as u64).to_be_bytes());
            hash.update(relative.as_bytes());
            hash.update([u8::from(is_executable(file)?)]);
            hash.update((content.len() as u64).to_be_bytes());
            hash.update(&content);
        }
        Ok(Self {
            root,
            manifest,
            content_hash: format!("sha256:{:x}", hash.finalize()),
            files: files.len(),
            bytes,
            source,
        })
    }
}

pub(crate) fn reload(
    root: &Path,
    id: &str,
    source: &PackageSource,
    medha_version: &str,
) -> Result<Package, Error> {
    match source {
        PackageSource::Plugin => Package::load(root, medha_version),
        PackageSource::HookSet => crate::hook_sets::load(root, id, medha_version),
        PackageSource::Settings => crate::compat::load_settings(root, id, medha_version)?
            .ok_or_else(|| Error::NotFound(id.to_string())),
    }
}

pub(crate) fn real_dir(requested: &Path) -> Result<PathBuf, Error> {
    let metadata = fs::symlink_metadata(requested)
        .map_err(|error| io_error("reading package directory", requested, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::UnsafePackage(format!(
            "{} must be a real directory, not a symlink or special file",
            requested.display()
        )));
    }
    requested
        .canonicalize()
        .map_err(|error| io_error("canonicalizing package directory", requested, error))
}

pub(crate) fn parse_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = read_text(path)?;
    toml::from_str(&text).map_err(|error| Error::Manifest(format!("{}: {error}", path.display())))
}

pub(crate) fn read_text(path: &Path) -> Result<String, Error> {
    let bytes = read_regular_bounded(path, MAX_MANIFEST_BYTES)?;
    String::from_utf8(bytes)
        .map_err(|error| Error::Manifest(format!("{} is not UTF-8: {error}", path.display())))
}

fn scan_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
        let mut entries = fs::read_dir(dir)
            .map_err(|error| io_error("reading package", dir, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_error("reading package entry", dir, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            // Version-control metadata is never part of what gets approved.
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| io_error("inspecting package entry", &path, error))?;
            if metadata.file_type().is_symlink() {
                return Err(Error::UnsafePackage(format!(
                    "package symlink is not allowed: {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                visit(&path, out)?;
            } else if metadata.is_file() {
                if metadata.len() > MAX_FILE_BYTES {
                    return Err(Error::UnsafePackage(format!(
                        "package file is larger than {} bytes: {}",
                        MAX_FILE_BYTES,
                        path.display()
                    )));
                }
                out.push(path);
                if out.len() > MAX_PACKAGE_FILES {
                    return Err(Error::UnsafePackage(format!(
                        "package contains more than {MAX_PACKAGE_FILES} files"
                    )));
                }
            } else {
                return Err(Error::UnsafePackage(format!(
                    "package contains a special file: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, &mut files)?;
    Ok(files)
}

fn portable_relative_path(path: &Path) -> Result<String, Error> {
    let mut parts = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(Error::UnsafePackage(format!(
                "package entry is not a portable relative path: {}",
                path.display()
            )));
        };
        let part = part.to_str().ok_or_else(|| {
            Error::UnsafePackage(format!(
                "package entry name is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn validate_component_files(root: &Path, manifest: &Manifest) -> Result<(), Error> {
    for component in &manifest.components {
        match component {
            ExtensionComponent::Skill { id, path } => {
                let directory = root.join(path);
                let metadata = fs::symlink_metadata(&directory).map_err(|error| {
                    io_error(
                        &format!("reading skill component '{id}'"),
                        &directory,
                        error,
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(Error::UnsafePackage(format!(
                        "skill component '{id}' path must be a real directory"
                    )));
                }
                read_regular_bounded(&directory.join("SKILL.md"), MAX_FILE_BYTES)?;
            }
            ExtensionComponent::Hook { entrypoint, .. } if entrypoint.shell => {}
            ExtensionComponent::Hook { id, entrypoint, .. } => {
                let program = root.join(&entrypoint.program);
                read_regular_bounded(&program, MAX_FILE_BYTES).map_err(|error| {
                    Error::UnsafePackage(format!(
                        "hook component '{id}' entrypoint is invalid: {error}"
                    ))
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = fs::metadata(&program)
                        .map_err(|error| io_error("reading hook permissions", &program, error))?
                        .permissions()
                        .mode();
                    if mode & 0o111 == 0 {
                        return Err(Error::UnsafePackage(format!(
                            "hook component '{id}' entrypoint is not executable"
                        )));
                    }
                }
            }
            ExtensionComponent::Action { .. } | ExtensionComponent::Mcp { .. } => {}
        }
    }
    Ok(())
}

fn read_regular_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspecting package file", path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::UnsafePackage(format!(
            "{} must be a real regular file",
            path.display()
        )));
    }
    if metadata.len() > limit {
        return Err(Error::UnsafePackage(format!(
            "{} is larger than {limit} bytes",
            path.display()
        )));
    }
    let mut content = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut content))
        .map_err(|error| io_error("reading package file", path, error))?;
    if content.len() as u64 > limit {
        return Err(Error::UnsafePackage(format!(
            "{} grew beyond {limit} bytes while being read",
            path.display()
        )));
    }
    Ok(content)
}

fn is_executable(path: &Path) -> Result<bool, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::symlink_metadata(path)
            .map_err(|error| io_error("reading package file mode", path, error))?
            .permissions()
            .mode();
        Ok(mode & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

pub(crate) fn copy_package(source: &Path, target: &Path) -> Result<(), Error> {
    let source_files = scan_files(source)?;
    fs::create_dir(target)
        .map_err(|error| io_error("creating plugin staging directory", target, error))?;
    let result = (|| {
        for source_file in source_files {
            let relative = source_file
                .strip_prefix(source)
                .expect("scanner only returns package descendants");
            let target_file = target.join(relative);
            if let Some(parent) = target_file.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    io_error("creating plugin package directory", parent, error)
                })?;
            }
            let content = read_regular_bounded(&source_file, MAX_FILE_BYTES)?;
            let executable = is_executable(&source_file)?;
            let mut target_handle = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target_file)
                .map_err(|error| io_error("creating plugin package file", &target_file, error))?;
            target_handle
                .write_all(&content)
                .and_then(|()| target_handle.sync_all())
                .map_err(|error| io_error("copying plugin package file", &target_file, error))?;
            // Only the exec bit survives: setuid, setgid, sticky, and group or
            // world write never enter the managed store.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if executable { 0o755 } else { 0o644 };
                fs::set_permissions(&target_file, fs::Permissions::from_mode(mode)).map_err(
                    |error| io_error("setting plugin file permissions", &target_file, error),
                )?;
            }
            #[cfg(not(unix))]
            let _ = executable;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(target);
    }
    result
}
