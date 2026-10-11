use super::*;
use std::time::Duration;

/// Whether `key` gets in right away while the others are held.
async fn enters(lanes: &Arc<WriteLanes>, key: &str) -> bool {
    tokio::time::timeout(Duration::from_millis(50), lanes.enter(key))
        .await
        .is_ok()
}

/// A long build in one tree holds up writes inside that tree and nothing else.
#[tokio::test]
async fn a_writer_waits_only_for_writers_it_overlaps() {
    let lanes = Arc::new(WriteLanes::default());
    let build = lanes.enter("path:/repo").await;

    assert!(!enters(&lanes, "path:/repo/src/main.rs").await);
    assert!(!enters(&lanes, "path:/").await);
    assert!(!enters(&lanes, WHOLE).await);
    assert!(
        !enters(&lanes, "some-future-key").await,
        "unknown keys overlap all"
    );
    assert!(
        enters(&lanes, "path:/repo-other/a.rs").await,
        "a sibling folder is free"
    );
    assert!(enters(&lanes, "path:/trees/child/src/main.rs").await);
    assert!(enters(&lanes, "memory:user:note").await);

    drop(build);
    let _edit = lanes.enter("path:/repo/src/main.rs").await;
    assert!(
        enters(&lanes, "path:/repo/src/lib.rs").await,
        "two files, two lanes"
    );
    assert!(!enters(&lanes, "path:/repo/src/main.rs").await);
}

/// A change that needs the whole workspace is not overtaken forever by small
/// ones, and a waiter that gives up does not block the line.
#[tokio::test]
async fn a_wide_writer_keeps_its_place_and_a_dropped_waiter_leaves_it() {
    let lanes = Arc::new(WriteLanes::default());
    let first = lanes.enter("path:/repo/a").await;
    let whole = tokio::spawn({
        let lanes = Arc::clone(&lanes);
        async move { lanes.enter(WHOLE).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !enters(&lanes, "path:/repo/b").await,
        "a later small write waits behind the wide one"
    );
    drop(first);
    let whole = tokio::time::timeout(Duration::from_secs(1), whole)
        .await
        .expect("the wide writer runs once the first is done")
        .unwrap();
    drop(whole);
    // The timed-out waiter above was dropped while waiting; nothing is left held.
    assert!(enters(&lanes, WHOLE).await);
}
