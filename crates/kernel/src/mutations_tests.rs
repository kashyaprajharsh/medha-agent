use crate::mutations::Guard;

#[test]
fn answered_notices_stay_quiet_and_capacity_forgets_the_oldest_review() {
    let guard = Guard::default();
    let first = guard
        .refuse(b"first", "file", "cannot back up", "command")
        .unwrap();
    assert_eq!(guard.notices().len(), 1);
    guard.dismiss(&first).unwrap();
    assert!(
        guard
            .refuse(b"first", "file", "cannot back up", "command")
            .is_err()
    );
    assert!(guard.notices().is_empty());
    let first = guard
        .refuse(b"oldest", "file", "blocked", "command")
        .unwrap();
    for number in 0u64..64 {
        guard
            .refuse(&number.to_le_bytes(), "file", "blocked", "command")
            .unwrap();
    }
    assert_eq!(guard.reviews().len(), 64);
    assert!(guard.reviews().iter().all(|review| review["id"] != first));
}
