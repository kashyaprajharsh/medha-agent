//! Bounded snapshots keep scanned project bytes identical to loaded bytes.

use policy::guard::{ScanVerdict, scan_package_with_modes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const VERSION: u32 = 1;
const FILES: usize = 256;
const FILE_BYTES: usize = 8 * 1024 * 1024;
const PACKAGE_BYTES: usize = 32 * 1024 * 1024;
const ENTRIES: usize = 2048;
/// Most bytes of passed packages kept in memory to avoid reading them again.
const PASSED_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Verdict {
    version: u32,
    hash: String,
    dangerous: bool,
    findings: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub files: BTreeMap<String, Vec<u8>>,
    pub hash: String,
    pub findings: Vec<String>,
}

fn memory() -> &'static Mutex<HashMap<String, Verdict>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Verdict>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Each file's path, size, change time and mode.
type Stamps = Vec<(String, u64, Option<std::time::SystemTime>, bool)>;

/// Packages that passed, with their files' stamps when they were read.
fn passed() -> &'static Mutex<HashMap<PathBuf, (Stamps, Snapshot)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Stamps, Snapshot)>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn stamp(
    metadata: &std::fs::Metadata,
    relative: String,
) -> (String, u64, Option<std::time::SystemTime>, bool) {
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    (
        relative,
        metadata.len(),
        metadata.modified().ok(),
        executable,
    )
}

#[derive(Default)]
struct Walk {
    files: BTreeMap<String, Vec<u8>>,
    modes: BTreeMap<String, bool>,
    stamps: Stamps,
    /// Off: only each file's stamp is taken, never its bytes.
    read: bool,
}

fn collect(
    root: &Path,
    dir: &Path,
    walk: &mut Walk,
    budget: &mut (usize, usize),
    depth: usize,
) -> Result<(), String> {
    if depth > 0 && std::fs::symlink_metadata(dir.join(".git")).is_ok() {
        return Err("project skill crosses a checkout boundary".into());
    }
    if dir.canonicalize().map_err(|e| e.to_string())? != dir {
        return Err("project skill crosses a symlink".into());
    }
    if depth > 32 {
        return Err("skill directory nesting exceeds 32 levels".into());
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        budget.1 += 1;
        if budget.1 > ENTRIES {
            return Err("project skill exceeds directory entry limit".into());
        }
        let path = entry.map_err(|e| e.to_string())?.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.is_symlink() {
            return Err(format!(
                "project skill crosses a symlink: {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect(root, &path, walk, budget, depth + 1)?;
        } else if metadata.is_file() {
            if walk.stamps.len() >= FILES || metadata.len() > FILE_BYTES as u64 {
                return Err("project skill exceeds package limits".into());
            }
            let relative = crate::portable_rel(path.strip_prefix(root).map_err(|e| e.to_string())?);
            if !walk.read {
                walk.stamps.push(stamp(&metadata, relative));
                continue;
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let file = options.open(&path).map_err(|e| e.to_string())?;
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file() {
                return Err("project skill contains a non-regular file".into());
            }
            let mut bytes = Vec::new();
            file.take(FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            budget.0 = budget.0.saturating_add(bytes.len());
            if bytes.len() > FILE_BYTES || budget.0 > PACKAGE_BYTES {
                return Err("project skill exceeds package limits".into());
            }
            if relative == "SKILL.md" && bytes.len() > crate::skills::MAX_SKILL_MD_BYTES {
                return Err("SKILL.md exceeds 128 KiB".into());
            }
            // The stamp of the file as it was read, not as it is later.
            let stamped = stamp(&metadata, relative.clone());
            walk.modes.insert(relative.clone(), stamped.3);
            walk.stamps.push(stamped);
            walk.files.insert(relative, bytes);
        } else {
            return Err("project skill contains a non-regular file".into());
        }
    }
    Ok(())
}

fn walk(root: &Path, read: bool) -> Result<Walk, String> {
    if root.canonicalize().map_err(|e| e.to_string())? != root {
        return Err("project skill path crosses a symlink".into());
    }
    let mut walk = Walk {
        read,
        ..Walk::default()
    };
    collect(root, root, &mut walk, &mut (0, 0), 0)?;
    walk.stamps.sort();
    Ok(walk)
}

fn read(root: &Path) -> Result<(Snapshot, BTreeMap<String, bool>, Stamps), String> {
    let Walk {
        files,
        modes,
        stamps,
        ..
    } = walk(root, true)?;
    let mut digest = Sha256::new();
    for (path, bytes) in &files {
        digest.update((path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
        digest.update([u8::from(modes[path])]);
    }
    let snapshot = Snapshot {
        files,
        hash: format!("{:x}", digest.finalize()),
        findings: Vec::new(),
    };
    Ok((snapshot, modes, stamps))
}

pub(crate) fn scan(
    root: &Path,
    cache: &Path,
    source: &dyn Fn(&Path) -> bool,
) -> Result<Snapshot, String> {
    if !source(root) {
        return Err("project skill requires checkout trust".into());
    }
    // A package that passed and whose files' stamps are unchanged is not read again.
    let stamps = walk(root, false)?.stamps;
    if let Some((known, snapshot)) = passed().lock().map_err(|e| e.to_string())?.get(root)
        && *known == stamps
    {
        return Ok(snapshot.clone());
    }
    let (mut snapshot, modes, stamps) = read(root)?;
    if !source(root) {
        return Err("project skill trust changed during its scan".into());
    }
    let key = format!("{VERSION}-{}", snapshot.hash);
    let cached = memory()
        .lock()
        .map_err(|e| e.to_string())?
        .get(&key)
        .cloned();
    let cache_file = cache.join(format!("{key}.json"));
    let cached = cached
        .or_else(|| {
            std::fs::read(&cache_file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Verdict>(&bytes).ok())
        })
        .filter(|cached| cached.version == VERSION && cached.hash == snapshot.hash);
    let fresh = cached.is_none();
    let verdict = cached.unwrap_or_else(|| {
        let report = scan_package_with_modes(
            snapshot
                .files
                .iter()
                .map(|(path, bytes)| (path.as_str(), bytes.as_slice(), modes[path])),
        );
        Verdict {
            version: VERSION,
            hash: snapshot.hash.clone(),
            dangerous: report.verdict == ScanVerdict::Dangerous,
            findings: crate::skills::format_findings(&report.findings),
        }
    });
    let mut memory = memory().lock().map_err(|e| e.to_string())?;
    if memory.len() >= 1024 {
        memory.clear();
    }
    memory.insert(key, verdict.clone());
    drop(memory);
    if fresh
        && std::fs::create_dir_all(cache).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&verdict)
    {
        let temporary: PathBuf = cache_file.with_extension(format!("tmp-{}", ulid::Ulid::new()));
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(&temporary, &cache_file);
        }
        let _ = std::fs::remove_file(temporary);
    }
    if verdict.dangerous {
        return Err(format!(
            "dangerous project skill blocked: {}",
            verdict.findings.join("; ")
        ));
    }
    snapshot.findings = verdict.findings;
    let mut passed = passed().lock().map_err(|e| e.to_string())?;
    passed.remove(root);
    let held =
        |(_, snapshot): &(Stamps, Snapshot)| snapshot.files.values().map(Vec::len).sum::<usize>();
    let total = passed.values().map(held).sum::<usize>();
    let size = snapshot.files.values().map(Vec::len).sum::<usize>();
    if passed.len() >= 256 || total + size > PASSED_BYTES {
        passed.clear();
    }
    if size <= PASSED_BYTES {
        passed.insert(root.to_path_buf(), (stamps, snapshot.clone()));
    }
    Ok(snapshot)
}

pub fn hash_project_skill(root: &Path) -> Result<(String, usize, usize), String> {
    let (snapshot, _, _) = read(root)?;
    Ok((
        snapshot.hash,
        snapshot.files.len(),
        snapshot.files.values().map(Vec::len).sum(),
    ))
}

pub(crate) fn scan_package(root: &Path) -> Result<policy::guard::ScanReport, String> {
    if std::fs::symlink_metadata(root)
        .map_err(|error| error.to_string())?
        .is_symlink()
    {
        return Err("skill package crosses a symlink".into());
    }
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let (snapshot, modes, _) = read(&root)?;
    Ok(scan_package_with_modes(snapshot.files.iter().map(
        |(path, bytes)| (path.as_str(), bytes.as_slice(), modes[path]),
    )))
}

#[cfg(test)]
#[path = "project_skill_scan_tests.rs"]
mod tests;
