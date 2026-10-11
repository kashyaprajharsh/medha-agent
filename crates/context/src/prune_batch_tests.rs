use super::PruneBatch;

#[test]
fn rejected_batches_leave_the_regrowth_guard_unchanged() {
    let batch = PruneBatch::default();
    assert!(!batch.accept(7_000, 6_501, 10_000, 6_000));
    assert_eq!(batch.threshold(), 0);
    assert!(batch.accept(7_000, 6_500, 10_000, 6_000));
    assert_eq!(batch.threshold(), 12_500);
    assert!(!batch.ready(12_499));
    assert!(batch.ready(12_500));
    assert!(!batch.accept(8_000, 8_001, 10_000, 6_000));
    assert_eq!(batch.threshold(), 12_500);
}

#[test]
fn restart_keeps_the_larger_reclaimed_runway() {
    let batch = PruneBatch::default();
    assert!(batch.accept(8_000, 1_000, 10_000, 6_000));
    let restored = PruneBatch::default();
    restored.restore(batch.threshold());
    assert!(!restored.ready(7_999));
    assert!(restored.ready(8_000));
    restored.restore(0);
    assert!(restored.ready(1_000));
}
