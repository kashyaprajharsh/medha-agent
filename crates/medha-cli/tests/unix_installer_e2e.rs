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
    fs::create_dir(&bin).unwrap();
    for name in [
        "awk", "chmod", "cp", "gzip", "install", "mkdir", "mktemp", "rm", "tar", "tr", "uname",
    ] {
        let target = find_command(name);
        symlink(target, bin.join(name)).unwrap();
    }

    if use_wget {
        write_executable(
            &bin.join("wget"),
            r#"#!/bin/sh
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
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) shift; out="$1" ;;
    -w) shift ;;
    http://*|https://*) url="$1" ;;
  esac
  shift
done
case "$url" in
  *.sha256)
    case "$TEST_CHECKSUM_MODE" in
      present) cp "$TEST_CHECKSUM" "$out"; printf '200'; exit 0 ;;
      missing) printf '404'; exit 0 ;;
      error) printf '503'; exit 7 ;;
      *) exit 9 ;;
    esac
    ;;
  *) cp "$TEST_ARCHIVE" "$out" ;;
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
    let checksum_path = case.path().join("asset.sha256");
    fs::write(&checksum_path, checksum).unwrap();
    let marker = case.path().join("sha256sum-used");
    let install_dir = case.path().join("installed");
    let bin = command_path(case.path(), with_sha256sum, false);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh");

    Command::new("/bin/sh")
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
        .env("PATH", OsString::from(bin))
        .output()
        .unwrap()
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
        let output = invoke(&case, &archive, "missing", "", false);
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
fn wget_fallback_distinguishes_a_missing_checksum_from_transport_failure() {
    let missing = TempDir::new().unwrap();
    let archive = missing.path().join("valid.tar.gz");
    make_archive(
        &archive,
        &[Entry {
            name: "medha",
            kind: EntryKind::Regular,
            body: b"wget-installed",
            link: "",
        }],
    );
    let output = invoke_with_wget(&missing, &archive, "missing", "");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read(missing.path().join("installed/medha")).unwrap(),
        b"wget-installed"
    );

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
