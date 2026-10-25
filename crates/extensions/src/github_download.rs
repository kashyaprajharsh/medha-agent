//! A GitHub repository at an exact commit, downloaded over HTTPS on a machine without Git.

use crate::{Error, io_error};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 500 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;

/// `owner` and `repo` of a `https://github.com/owner/repo` source.
pub(crate) fn repository(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| (owner.to_owned(), repo.to_owned()))
}

/// Unpacks the repository at `git_ref` (its default branch when none) into `dir` and returns the full commit.
pub(crate) fn fetch(
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
    dir: &Path,
) -> Result<String, Error> {
    let api = format!("https://api.github.com/repos/{owner}/{repo}");
    let wanted = git_ref.unwrap_or("HEAD");
    // The same full commit a clone records, so approval and updates compare alike.
    let commit = get(
        &format!("{api}/commits/{wanted}"),
        "application/vnd.github.sha",
        4096,
    )?;
    let commit = String::from_utf8_lossy(&commit).trim().to_owned();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Source(format!(
            "GitHub named no commit for {owner}/{repo} at {wanted}"
        )));
    }
    let archive = get(
        &format!("{api}/zipball/{commit}"),
        "application/vnd.github+json",
        MAX_ARCHIVE_BYTES,
    )?;
    unpack(&archive, dir)?;
    Ok(commit)
}

/// One GET, at most `limit` bytes; its own thread and runtime, since callers may already run inside one.
fn get(url: &str, accept: &str, limit: u64) -> Result<Vec<u8>, Error> {
    let (url, accept) = (url.to_owned(), accept.to_owned());
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| Error::Source(format!("could not start a download: {error}")))?;
        runtime.block_on(async {
            let failed = |error: reqwest::Error| {
                Error::Source(format!("GitHub could not be reached: {error}"))
            };
            let mut response = reqwest::Client::builder()
                .user_agent("medha")
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(failed)?
                .get(&url)
                .header(reqwest::header::ACCEPT, accept)
                .send()
                .await
                .map_err(failed)?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Source(match status.as_u16() {
                    404 | 422 => "GitHub has no public repository or ref by that name; a private \
                                  repository needs Git (git-scm.com)"
                        .into(),
                    403 | 429 => "GitHub's hourly download limit was reached; try again later, or \
                                  install Git (git-scm.com)"
                        .into(),
                    code => format!("GitHub answered {code} for {url}"),
                }));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(failed)? {
                body.extend_from_slice(&chunk);
                if body.len() as u64 > limit {
                    return Err(Error::UnsafePackage(format!(
                        "the download from {url} is larger than {} MB",
                        limit / (1024 * 1024)
                    )));
                }
            }
            Ok(body)
        })
    })
    .join()
    .map_err(|_| Error::Source("the download stopped unexpectedly".into()))?
}

/// The archive's files under `dir`, without GitHub's `owner-repo-commit/` wrapper folder.
fn unpack(bytes: &[u8], dir: &Path) -> Result<(), Error> {
    let unreadable = |error: zip::result::ZipError| {
        Error::Source(format!("GitHub sent an unreadable archive: {error}"))
    };
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(unreadable)?;
    if archive.len() > MAX_ENTRIES {
        return Err(Error::UnsafePackage(format!(
            "the repository has more than {MAX_ENTRIES} files"
        )));
    }
    let (mut unpacked, mut links) = (0u64, Vec::new());
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(unreadable)?;
        let path = entry.enclosed_name().ok_or_else(|| {
            Error::UnsafePackage(format!(
                "archive entry {} leaves its folder",
                entry.name().as_deref().unwrap_or("?")
            ))
        })?;
        let relative: PathBuf = path.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dir.join(&relative);
        if entry.is_symlink() {
            let mut pointee = String::new();
            entry
                .read_to_string(&mut pointee)
                .map_err(|error| io_error("reading link", &target, error))?;
            links.push((relative, pointee));
            continue;
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|error| io_error("unpacking", &target, error))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_error("unpacking", parent, error))?;
        }
        let mut file = std::fs::File::create(&target)
            .map_err(|error| io_error("unpacking", &target, error))?;
        unpacked += std::io::copy(
            &mut (&mut entry).take(MAX_UNPACKED_BYTES - unpacked + 1),
            &mut file,
        )
        .map_err(|error| io_error("unpacking", &target, error))?;
        if unpacked > MAX_UNPACKED_BYTES {
            return Err(Error::UnsafePackage(
                "the repository unpacks to more than 500 MB".into(),
            ));
        }
    }
    // As with a clone: a link to something inside the repository becomes a copy, any other is dropped.
    for (link, pointee) in links {
        let Some(target) = inside(&link, &pointee) else {
            continue;
        };
        if dir.join(&target).exists() {
            crate::sources::copy_tree(&dir.join(&target), &dir.join(&link))?;
        }
    }
    Ok(())
}

/// Where a link's text points, relative to the repository root, if it stays inside it.
fn inside(link: &Path, pointee: &str) -> Option<PathBuf> {
    let mut resolved: Vec<Component> = link.parent()?.components().collect();
    for part in Path::new(pointee).components() {
        match part {
            Component::Normal(_) => resolved.push(part),
            Component::ParentDir => {
                resolved.pop()?;
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let target: PathBuf = resolved.into_iter().collect();
    (!target.as_os_str().is_empty() && !link.starts_with(&target)).then_some(target)
}

#[cfg(test)]
#[path = "github_download_tests.rs"]
mod tests;
