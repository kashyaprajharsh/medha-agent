use super::*;

const HOUR: u64 = 60 * 60;

fn book() -> (tempfile::TempDir, Pairing) {
    let folder = tempfile::tempdir().unwrap();
    let pairing = Pairing::at(folder.path().join("pairing.json"));
    (folder, pairing)
}

#[test]
fn a_code_lets_in_only_its_own_person_once_and_never_after_an_hour() {
    let (_folder, pairing) = book();
    let code = pairing
        .request_at("telegram", "42", "Asha", 1_000)
        .unwrap()
        .unwrap();
    assert_eq!(code.len(), 8);
    assert!(code.bytes().all(|byte| ALPHABET.contains(&byte)));
    assert!(!pairing.is_approved("telegram", "42").unwrap());
    assert!(
        pairing
            .approve_at("discord", &code, 1_001)
            .unwrap()
            .is_err(),
        "another platform's code"
    );

    let spaced = code
        .chars()
        .map(|c| format!("{c} "))
        .collect::<String>()
        .to_lowercase();
    let person = pairing
        .approve_at("telegram", &spaced, 1_002)
        .unwrap()
        .unwrap();
    assert_eq!((person.user.as_str(), person.name.as_str()), ("42", "Asha"));
    assert!(pairing.is_approved("telegram", "42").unwrap());
    assert!(
        pairing
            .approve_at("telegram", &code, 1_003)
            .unwrap()
            .is_err(),
        "a code works once"
    );

    let late = pairing
        .request_at("telegram", "7", "Ravi", 1_000)
        .unwrap()
        .unwrap();
    assert!(
        pairing
            .approve_at("telegram", &late, 1_000 + HOUR)
            .unwrap()
            .is_err()
    );
    assert!(!pairing.is_approved("telegram", "7").unwrap());
}

#[test]
fn the_file_never_holds_a_code() {
    let (folder, pairing) = book();
    let code = pairing
        .request_at("telegram", "42", "Asha", 1_000)
        .unwrap()
        .unwrap();
    let written = std::fs::read_to_string(folder.path().join("pairing.json")).unwrap();
    assert!(!written.contains(&code));
}

#[test]
fn one_person_asks_once_per_ten_minutes_and_three_wait_per_platform() {
    let (_folder, pairing) = book();
    assert!(
        pairing
            .request_at("telegram", "1", "", 0)
            .unwrap()
            .is_some()
    );
    assert!(
        pairing
            .request_at("telegram", "1", "", 599)
            .unwrap()
            .is_none()
    );
    assert!(
        pairing
            .request_at("telegram", "2", "", 1)
            .unwrap()
            .is_some()
    );
    assert!(
        pairing
            .request_at("telegram", "3", "", 2)
            .unwrap()
            .is_some()
    );
    assert!(
        pairing
            .request_at("telegram", "4", "", 3)
            .unwrap()
            .is_none(),
        "a fourth waits on one platform"
    );
    assert!(pairing.request_at("discord", "4", "", 3).unwrap().is_some());
    // The first person's new code replaces their old one rather than adding a fourth.
    assert!(
        pairing
            .request_at("telegram", "1", "", 600)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        pairing
            .waiting_at(600)
            .unwrap()
            .iter()
            .filter(|w| w.platform == "telegram")
            .count(),
        3
    );
}

#[test]
fn five_wrong_codes_lock_the_platform_for_an_hour_even_against_a_right_code() {
    let (_folder, pairing) = book();
    let code = pairing
        .request_at("telegram", "42", "", 0)
        .unwrap()
        .unwrap();
    for attempt in 0..5 {
        assert!(
            pairing
                .approve_at("telegram", "WRONGCDE", attempt)
                .unwrap()
                .is_err()
        );
    }
    let locked = pairing.approve_at("telegram", &code, 10).unwrap();
    assert!(locked.unwrap_err().contains("locked"));
    assert!(
        pairing
            .request_at("telegram", "9", "", 10)
            .unwrap()
            .is_none(),
        "no new codes while locked"
    );
    let fresh = pairing
        .request_at("telegram", "42", "", HOUR + 10)
        .unwrap()
        .unwrap();
    assert!(
        pairing
            .approve_at("telegram", &fresh, HOUR + 11)
            .unwrap()
            .is_ok()
    );
}

#[test]
fn a_right_code_resets_the_wrong_code_count() {
    let (_folder, pairing) = book();
    for attempt in 0..4 {
        assert!(
            pairing
                .approve_at("telegram", "WRONGCDE", attempt)
                .unwrap()
                .is_err()
        );
    }
    let code = pairing
        .request_at("telegram", "42", "", 5)
        .unwrap()
        .unwrap();
    assert!(pairing.approve_at("telegram", &code, 6).unwrap().is_ok());
    let next = pairing
        .request_at("telegram", "43", "", 7)
        .unwrap()
        .unwrap();
    assert!(
        pairing
            .approve_at("telegram", "WRONGCDE", 8)
            .unwrap()
            .is_err()
    );
    assert!(
        pairing.approve_at("telegram", &next, 9).unwrap().is_ok(),
        "one typo after a success does not lock"
    );
}

#[test]
fn revoking_shuts_the_person_out() {
    let (_folder, pairing) = book();
    let code = pairing
        .request_at("telegram", "42", "Asha", 0)
        .unwrap()
        .unwrap();
    pairing.approve_at("telegram", &code, 1).unwrap().unwrap();
    assert!(pairing.revoke("telegram", "42").unwrap());
    assert!(!pairing.is_approved("telegram", "42").unwrap());
    assert!(!pairing.revoke("telegram", "42").unwrap());
}

#[cfg(unix)]
#[test]
fn the_file_is_readable_only_by_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let (folder, pairing) = book();
    pairing.request_at("telegram", "42", "", 0).unwrap();
    let mode = std::fs::metadata(folder.path().join("pairing.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
