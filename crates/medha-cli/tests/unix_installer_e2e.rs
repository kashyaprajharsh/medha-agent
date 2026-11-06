#![cfg(unix)]

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum EntryKind {
    Regular,
    Symlink,
    Fifo,
}

struct Entry<'a> {
    name: &'a str,
    kind: EntryKind,
    body: &'a [u8],
    link: &'a str,
}

fn octal(field: &mut [u8], value: u64) {
    let rendered = format!("{value:0width$o}\0", width = field.len() - 1);
    field.copy_from_slice(rendered.as_bytes());
}

fn append_entry(output: &mut Vec<u8>, entry: &Entry<'_>) {
    assert!(entry.name.len() <= 100);
    assert!(entry.link.len() <= 100);

    let mut header = [0_u8; 512];
    header[..entry.name.len()].copy_from_slice(entry.name.as_bytes());
    octal(&mut header[100..108], 0o755);
    octal(&mut header[108..116], 0);
    octal(&mut header[116..124], 0);
    let size = match entry.kind {
        EntryKind::Regular => entry.body.len() as u64,
        EntryKind::Symlink | EntryKind::Fifo => 0,
    };
    octal(&mut header[124..136], size);
    octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = match entry.kind {
        EntryKind::Regular => b'0',
        EntryKind::Symlink => b'2',
        EntryKind::Fifo => b'6',
    };
    header[157..157 + entry.link.len()].copy_from_slice(entry.link.as_bytes());
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");

    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    let rendered = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(rendered.as_bytes());
    output.extend_from_slice(&header);

    if matches!(entry.kind, EntryKind::Regular) {
        output.extend_from_slice(entry.body);
        let padding = (512 - (entry.body.len() % 512)) % 512;
        output.resize(output.len() + padding, 0);
    }
}

fn make_archive(path: &Path, entries: &[Entry<'_>]) {
    let mut tar = Vec::new();
    for entry in entries {
        append_entry(&mut tar, entry);
    }
    tar.resize(tar.len() + 1024, 0);

    let file = fs::File::create(path).unwrap();
    let mut encoder = GzEncoder::new(file, Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap();
}

fn find_command(name: &str) -> PathBuf {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("test host is missing required command {name}"))
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn command_path(root: &Path, with_sha256sum: bool, use_wget: bool) -> PathBuf {
    let bin = root.join("bin");
    if bin.exists() {
        return bin;
    }
    fs::create_dir(&bin).unwrap();
    for name in [
        "awk", "cat", "chmod", "cp", "grep", "gzip", "ln", "ls", "mkdir", "mktemp", "mv", "ps",
        "rm", "sleep", "tail", "tar", "tr", "uname",
    ] {
        let target = find_command(name);
        symlink(target, bin.join(name)).unwrap();
    }

    if use_wget {
        write_executable(
            &bin.join("wget"),
            r#"#!/bin/sh
[ -n "${TEST_ARGS_LOG:-}" ] && printf '%s\n' "$*" >> "$TEST_ARGS_LOG"
out=
headers=false
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -qO) shift; out="$1" ;;
    -S) headers=true ;;
    http://*|https://*) url="$1" ;;
  esac
  shift
done
case "$url" in
  *.sha256)
    case "$TEST_CHECKSUM_MODE" in
      present) cp "$TEST_CHECKSUM" "$out"; exit 0 ;;
      missing) [ "$headers" = true ] && printf '  HTTP/1.1 404 Not Found\n' >&2; exit 8 ;;
      error) [ "$headers" = true ] && printf '  HTTP/1.1 503 Unavailable\n' >&2; exit 8 ;;
      *) exit 9 ;;
    esac
    ;;
  *) cp "$TEST_ARCHIVE" "$out" ;;
esac
"#,
        );
    } else {
        write_executable(
            &bin.join("curl"),
            r#"#!/bin/sh
out=
url=
status=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) shift; out="$1" ;;
    -w) shift; status=true ;;
    http://*|https://*) url="$1" ;;
  esac
  shift
done
[ -n "${TEST_URL_LOG:-}" ] && printf '%s\n' "$url" >> "$TEST_URL_LOG"
# The mirror behaves as TEST_MIRROR_MODE says: down, missing its files, or behind a bad certificate.
case "$url" in
  https://mirror.test/*)
    case "${TEST_MIRROR_MODE:-}" in
      down) exit 7 ;;
      missing) [ "$status" = true ] && printf '404'; [ "$status" = true ] && exit 0; exit 22 ;;
      certificate) exit 60 ;;
      blocked) [ "$status" = true ] && printf '403'; exit 0 ;;
    esac
    ;;
esac
case "$url" in
  *.sha256)
    case "$TEST_CHECKSUM_MODE" in
      present) cp "$TEST_CHECKSUM" "$out"; printf '200'; exit 0 ;;
      missing) printf '404'; exit 0 ;;
      error) printf '503'; exit 7 ;;
      *) exit 9 ;;
    esac
    ;;
  *) cp "$TEST_ARCHIVE" "$out"; [ "$status" = true ] && printf '200'; exit 0 ;;
esac
"#,
        );
    }
    if with_sha256sum {
        write_executable(
            &bin.join("sha256sum"),
            r#"#!/bin/sh
printf '%s\n' invoked > "$TEST_SHA_MARKER"
printf '%s  %s\n' "$TEST_ACTUAL" "$1"
"#,
        );
    }
    bin
}

fn sha256(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn invoke(
    case: &TempDir,
    archive: &Path,
    checksum_mode: &str,
    checksum: &str,
    with_sha256sum: bool,
) -> Output {
    installer(case, archive, checksum_mode, checksum, with_sha256sum)
        .output()
        .unwrap()
}

fn installer(
    case: &TempDir,
    archive: &Path,
    checksum_mode: &str,
    checksum: &str,
    with_sha256sum: bool,
) -> Command {
    let checksum_path = case.path().join("asset.sha256");
    fs::write(&checksum_path, checksum).unwrap();
    let marker = case.path().join("sha256sum-used");
    let install_dir = case.path().join("installed");
    let bin = command_path(case.path(), with_sha256sum, false);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh");

    let mut command = Command::new("/bin/sh");
    command
        .arg(script)
        .env("MEDHA_REPO", "example/medha")
        .env("MEDHA_VERSION", "v-test")
        .env("MEDHA_INSTALL_DIR", &install_dir)
        .env("TEST_ARCHIVE", archive)
        .env("TEST_CHECKSUM", checksum_path)
        .env("TEST_CHECKSUM_MODE", checksum_mode)
        .env("TEST_ACTUAL", sha256(archive))
        .env("TEST_SHA_MARKER", marker)
        .env("HOME", case.path())
        .env("PATH", OsString::from(bin));
    command
}

/// Installs `body` as `version` into `case`, the way a release reaches a user.
fn install_version(case: &TempDir, version: &str, body: &[u8]) -> Output {
    let archive = case.path().join(format!("{version}.tar.gz"));
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body,
            link: "",
        }],
    );
    let checksum = format!("{}  medha-test.tar.gz\n", sha256(&archive));
    installer(case, &archive, "present", &checksum, true)
        .env("MEDHA_VERSION", version)
        .output()
        .unwrap()
}

fn versions(case: &TempDir) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(case.path().join(".local/lib/medha/versions"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn invoke_with_wget(case: &TempDir, archive: &Path, checksum_mode: &str, checksum: &str) -> Output {
    let checksum_path = case.path().join("asset.sha256");
    fs::write(&checksum_path, checksum).unwrap();
    let install_dir = case.path().join("installed");
    let bin = command_path(case.path(), false, true);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh");

    Command::new("/bin/sh")
        .arg(script)
        .env("MEDHA_REPO", "example/medha")
        .env("MEDHA_VERSION", "v-test")
        .env("MEDHA_INSTALL_DIR", &install_dir)
        .env("TEST_ARCHIVE", archive)
        .env("TEST_CHECKSUM", checksum_path)
        .env("TEST_CHECKSUM_MODE", checksum_mode)
        .env("HOME", case.path())
        .env("PATH", OsString::from(bin))
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unix_installer_verifies_and_rejects_ambiguous_or_unsafe_inputs() {
    let valid_case = TempDir::new().unwrap();
    let valid_archive = valid_case.path().join("valid.tar.gz");
    make_archive(
        &valid_archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"test-medha-binary",
            link: "",
        }],
    );
    let digest = sha256(&valid_archive);
    let output = invoke(
        &valid_case,
        &valid_archive,
        "present",
        &format!("{}  medha-test.tar.gz\r\n", digest.to_uppercase()),
        true,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read(valid_case.path().join("installed/medha")).unwrap(),
        b"test-medha-binary"
    );
    assert!(valid_case.path().join("sha256sum-used").is_file());

    let missing_verifier = TempDir::new().unwrap();
    let archive = missing_verifier.path().join("valid.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"binary",
            link: "",
        }],
    );
    let digest = sha256(&archive);
    let output = invoke(&missing_verifier, &archive, "present", &digest, false);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("requires sha256sum or shasum"));

    let mismatch = TempDir::new().unwrap();
    let archive = mismatch.path().join("valid.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"binary",
            link: "",
        }],
    );
    let output = invoke(&mismatch, &archive, "present", &"0".repeat(64), true);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("checksum mismatch"));

    let transient = TempDir::new().unwrap();
    let archive = transient.path().join("valid.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"binary",
            link: "",
        }],
    );
    let output = invoke(&transient, &archive, "error", "", false);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("checksum download failed"));

    let hostile_cases = [
        (
            "duplicate",
            vec![
                Entry {
                    name: "medha",
                    kind: EntryKind::Regular,
                    body: b"first",
                    link: "",
                },
                Entry {
                    name: "medha",
                    kind: EntryKind::Regular,
                    body: b"second",
                    link: "",
                },
            ],
        ),
        (
            "traversal",
            vec![Entry {
                name: "../medha",
                kind: EntryKind::Regular,
                body: b"binary",
                link: "",
            }],
        ),
        (
            "absolute",
            vec![Entry {
                name: "/medha",
                kind: EntryKind::Regular,
                body: b"binary",
                link: "",
            }],
        ),
        (
            "symlink",
            vec![Entry {
                name: "medha",
                kind: EntryKind::Symlink,
                body: b"",
                link: "/tmp/attacker",
            }],
        ),
        (
            "fifo",
            vec![Entry {
                name: "medha",
                kind: EntryKind::Fifo,
                body: b"",
                link: "",
            }],
        ),
    ];
    for (name, entries) in hostile_cases {
        let case = TempDir::new().unwrap();
        let archive = case.path().join(format!("{name}.tar.gz"));
        make_archive(&archive, &entries);
        let digest = sha256(&archive);
        let output = invoke(&case, &archive, "present", &digest, true);
        assert!(
            !output.status.success(),
            "{name} archive unexpectedly installed"
        );
        assert!(
            !case.path().join("installed/medha").exists(),
            "{name} archive wrote the destination binary"
        );
    }
}

#[test]
fn a_missing_checksum_is_refused_with_curl_and_with_wget() {
    for via_wget in [false, true] {
        let missing = TempDir::new().unwrap();
        let archive = missing.path().join("valid.tar.gz");
        make_archive(
            &archive,
            &[Entry {
                name: "medha",
                kind: EntryKind::Regular,
                body: b"must-not-install",
                link: "",
            }],
        );
        let output = if via_wget {
            invoke_with_wget(&missing, &archive, "missing", "")
        } else {
            invoke(&missing, &archive, "missing", "", true)
        };
        assert!(!output.status.success(), "installed without a checksum");
        assert!(stderr(&output).contains("no checksum published"));
        assert!(!missing.path().join("installed/medha").exists());
    }

    let transient = TempDir::new().unwrap();
    let archive = transient.path().join("valid.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"must-not-install",
            link: "",
        }],
    );
    let output = invoke_with_wget(&transient, &archive, "error", "");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("checksum download failed"));
    assert!(!transient.path().join("installed/medha").exists());
}

/// Every wget download refuses a redirect to plain http, as curl's do.
#[test]
fn wget_downloads_are_https_only() {
    let case = TempDir::new().unwrap();
    let archive = case.path().join("valid.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"medha",
            link: "",
        }],
    );
    let log = case.path().join("wget-args");
    let bin = command_path(case.path(), false, true);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh");
    let output = Command::new("/bin/sh")
        .arg(script)
        .env("MEDHA_REPO", "example/medha")
        .env("MEDHA_VERSION", "v-test")
        .env("MEDHA_INSTALL_DIR", case.path().join("installed"))
        .env("TEST_ARCHIVE", &archive)
        .env("TEST_CHECKSUM", &archive)
        .env("TEST_CHECKSUM_MODE", "missing")
        .env("TEST_ARGS_LOG", &log)
        .env("HOME", case.path())
        .env("PATH", OsString::from(bin))
        .output()
        .unwrap();
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(calls.lines().count() >= 2, "{calls}\n{}", stderr(&output));
    for call in calls.lines() {
        assert!(call.contains("--https-only"), "{call}");
    }
}

/// A download cut off part way through runs none of the script.
#[test]
fn a_truncated_installer_runs_nothing() {
    let case = TempDir::new().unwrap();
    let full =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh")).unwrap();
    let cut = full.find("mkdir -p \"$STORE/versions\"").unwrap();
    let truncated = &full[..full[cut..].find('\n').unwrap() + cut + 1];
    let bin = command_path(case.path(), true, false);
    let mut shell = Command::new("/bin/sh")
        .env("MEDHA_REPO", "example/medha")
        .env("MEDHA_VERSION", "v-test")
        .env("MEDHA_INSTALL_DIR", case.path().join("installed"))
        .env("HOME", case.path())
        .env("PATH", OsString::from(bin))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    shell
        .stdin
        .take()
        .unwrap()
        .write_all(truncated.as_bytes())
        .unwrap();
    let output = shell.wait_with_output().unwrap();
    assert!(!output.status.success(), "a cut-off script exited cleanly");
    assert!(
        !case.path().join("installed").exists(),
        "{}",
        stderr(&output)
    );
    assert!(!case.path().join(".local").exists(), "{}", stderr(&output));
}

/// An update while medha runs as the backend must leave that process and its file untouched.
#[test]
fn an_update_never_writes_over_a_running_medha() {
    let case = TempDir::new().unwrap();
    let link = case.path().join("installed/medha");
    let sleeper = case.path().join("sleeper");
    fs::copy(find_command("sleep"), &sleeper).unwrap();
    // macOS kills a copied system program until it carries its own signature.
    #[cfg(target_os = "macos")]
    assert!(
        Command::new("codesign")
            .args(["-s", "-", "-f"])
            .arg(&sleeper)
            .status()
            .unwrap()
            .success()
    );
    let sleeper = fs::read(&sleeper).unwrap();
    let first = install_version(&case, "v-one", &sleeper);
    assert!(first.status.success(), "{}", stderr(&first));
    let mut running = Command::new(&link).arg("30").spawn().unwrap();

    let second = install_version(&case, "v-two", b"two");
    let still_running = running.try_wait().unwrap().is_none();
    let _ = running.kill();
    let _ = running.wait();
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(still_running, "installing stopped the running medha");
    assert_eq!(fs::read(&link).unwrap(), b"two");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(versions(&case), ["v-one", "v-two"]);

    let third = install_version(&case, "v-three", b"three");
    assert!(third.status.success(), "{}", stderr(&third));
    assert_eq!(
        versions(&case),
        ["v-three", "v-two"],
        "only the current and previous version stay"
    );
}

/// An update told the running copy's version store installs there, so the relaunch finds the new version.
#[test]
fn an_update_installs_into_the_version_store_it_is_given() {
    let case = TempDir::new().unwrap();
    let store = case.path().join("system-store");
    let archive = case.path().join("v-one.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"one",
            link: "",
        }],
    );
    let checksum = format!("{}  medha-test.tar.gz\n", sha256(&archive));
    let output = installer(&case, &archive, "present", &checksum, true)
        .env("MEDHA_VERSION", "v-one")
        .env("MEDHA_STORE", &store)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read(store.join("versions/v-one/medha")).unwrap(),
        b"one"
    );
    let link = fs::canonicalize(case.path().join("installed/medha")).unwrap();
    assert!(
        link.starts_with(fs::canonicalize(&store).unwrap()),
        "{link:?}"
    );
}

/// A binary written by an older installer is replaced by the versioned link, not left beside it.
#[test]
fn an_existing_plain_install_becomes_the_versioned_link() {
    let case = TempDir::new().unwrap();
    let link = case.path().join("installed/medha");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    fs::write(&link, b"old layout").unwrap();
    let output = install_version(&case, "v-new", b"new");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&link).unwrap(), b"new");
}

/// A lock whose installer is gone must not block every later install.
#[test]
fn a_lock_left_by_a_crashed_install_is_taken_over() {
    let case = TempDir::new().unwrap();
    let lock = case.path().join(".local/lib/medha/install.lock");
    fs::create_dir_all(&lock).unwrap();
    let mut gone = Command::new(find_command("true")).spawn().unwrap();
    gone.wait().unwrap();
    fs::write(lock.join("pid"), gone.id().to_string()).unwrap();
    let output = install_version(&case, "v-after", b"after");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!lock.exists(), "the lock outlived the install");
}

/// The version becomes a folder name, so anything but a plain tag is refused before download.
#[test]
fn a_version_that_is_not_a_tag_is_refused() {
    let case = TempDir::new().unwrap();
    let output = install_version(&case, "../escape", b"never");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("not a release tag"));
    assert!(!case.path().join("installed/medha").exists());
}

/// Installs through the download mirror in `mode`; gives the run and every address it asked for.
fn install_mirrored(mode: &str) -> (TempDir, Output, Vec<String>) {
    let case = TempDir::new().unwrap();
    let archive = case.path().join("release.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"mirrored",
            link: "",
        }],
    );
    let checksum = format!("{}  medha-test.tar.gz\n", sha256(&archive));
    let log = case.path().join("urls.log");
    let output = installer(&case, &archive, "present", &checksum, true)
        .env_remove("MEDHA_REPO")
        .env("MEDHA_MIRROR", "https://mirror.test")
        .env("TEST_MIRROR_MODE", mode)
        .env("TEST_URL_LOG", &log)
        .output()
        .unwrap();
    let asked = fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (case, output, asked)
}

/// The archive comes from the mirror, and its checksum from GitHub, a second host.
#[test]
fn a_release_comes_from_the_mirror_and_is_checked_against_github() {
    let (case, output, asked) = install_mirrored("serves");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read(case.path().join("installed/medha")).unwrap(),
        b"mirrored"
    );
    let archive = asked
        .iter()
        .filter(|url| url.ends_with(".tar.gz"))
        .collect::<Vec<_>>();
    assert_eq!(archive.len(), 1, "{asked:?}");
    assert!(
        archive[0].starts_with("https://mirror.test/v-test/"),
        "{asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|url| url.starts_with("https://github.com/") && url.ends_with(".tar.gz.sha256")),
        "{asked:?}"
    );
}

/// A mirror that is down, lacks the files, or refuses the network hands over to GitHub, and is
/// asked once rather than retried.
#[test]
fn a_mirror_that_cannot_serve_hands_over_to_github() {
    for mode in ["down", "missing", "blocked"] {
        let (case, output, asked) = install_mirrored(mode);
        assert!(output.status.success(), "{mode}: {}", stderr(&output));
        assert_eq!(
            fs::read(case.path().join("installed/medha")).unwrap(),
            b"mirrored",
            "{mode}"
        );
        let mirror = asked
            .iter()
            .filter(|url| url.starts_with("https://mirror.test/"))
            .count();
        assert_eq!(
            mirror, 1,
            "{mode}: the mirror was asked {mirror} times: {asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|url| url.starts_with("https://github.com/") && url.ends_with(".tar.gz")),
            "{mode}: {asked:?}"
        );
    }
}

/// A certificate that does not check out is someone in the way: the install stops and no other host is tried.
#[test]
fn a_mirror_with_a_bad_certificate_stops_the_install() {
    let (case, output, asked) = install_mirrored("certificate");
    assert!(!output.status.success());
    assert!(!case.path().join("installed/medha").exists());
    assert!(
        !asked
            .iter()
            .any(|url| url.starts_with("https://github.com/") && url.ends_with(".tar.gz")),
        "fell back after a bad certificate: {asked:?}"
    );
}
