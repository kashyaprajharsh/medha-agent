//! Activity animation, in one place.
//!
//! Two tiers, so concurrent indicators do not compete: [`primary`] for whatever
//! the user is waiting on, [`secondary`] at half speed for work running
//! alongside. Both are single-width so they never disturb the layout, and both
//! are pure functions of the frame counter — the view stays stateless.

/// What the user is waiting on: thinking, compacting. A light arc sweeping a
/// circle, deliberately not the braille speckle every other CLI ships.
const PRIMARY: [&str; 6] = ["◜", "◠", "◝", "◞", "◡", "◟"];

/// Work happening alongside: sub-agents, background tasks. Filling and emptying
/// rather than spinning, so it reads as ambient next to the primary sweep.
const SECONDARY: [&str; 6] = ["◌", "◍", "◐", "●", "◐", "◍"];

/// How many ticks each secondary frame holds — half the primary's pace, so the
/// background indicator never draws the eye off the foreground one.
const SECONDARY_DIVISOR: u64 = 4;

pub(super) fn primary(frame: u64) -> &'static str {
    PRIMARY[(frame as usize) % PRIMARY.len()]
}

pub(super) fn secondary(frame: u64) -> &'static str {
    SECONDARY[((frame / SECONDARY_DIVISOR) as usize) % SECONDARY.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_tiers_cycle_without_panicking_on_any_frame() {
        // The counter wraps, so every arm must be reachable from any value.
        for frame in [0, 1, 5, 6, 23, 24, u64::MAX] {
            assert_eq!(primary(frame).chars().count(), 1);
            assert_eq!(secondary(frame).chars().count(), 1);
        }
    }

    #[test]
    fn the_secondary_tier_runs_slower_than_the_primary() {
        // A background indicator that moved at the same rate would compete with
        // the one the user is actually waiting on.
        let primary_changes = (0..24).filter(|f| primary(*f) != primary(f + 1)).count();
        let secondary_changes = (0..24)
            .filter(|f| secondary(*f) != secondary(f + 1))
            .count();
        assert!(
            secondary_changes < primary_changes,
            "secondary {secondary_changes} should change less often than primary {primary_changes}"
        );
    }
}
