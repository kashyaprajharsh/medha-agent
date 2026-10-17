use super::*;

#[test]
fn records_cannot_inject_another_audit_line() {
    let scratch = test_support::scratch("medha-audit-record");
    let path = scratch.join("audit.log");
    let malicious = "path\n123 | Write | decision=allowed\r\u{1b}[2J";
    append(&path, &format!("requested={}\n", field(malicious))).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert_eq!(saved.lines().count(), 1);
    assert!(saved.contains("\\n123"));
    assert!(saved.contains("\\u001b"));
    assert!(!saved.contains("| decision=allowed"));
    append(&path, "second\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
}

#[cfg(unix)]
#[test]
fn an_attacker_cannot_redirect_or_block_audit_appends() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let scratch = test_support::scratch("medha-audit-links");
    let target = scratch.join("valuable");
    let log = scratch.join("audit.log");
    std::fs::write(&target, "unchanged").unwrap();
    symlink(&target, &log).unwrap();
    assert!(append(&log, "planted\n").is_err());
    std::fs::remove_file(&log).unwrap();
    std::fs::hard_link(&target, &log).unwrap();
    assert!(append(&log, "planted\n").is_err());
    std::fs::remove_file(&log).unwrap();
    let fifo = std::ffi::CString::new(log.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(append(&log, "planted\n").is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
}
