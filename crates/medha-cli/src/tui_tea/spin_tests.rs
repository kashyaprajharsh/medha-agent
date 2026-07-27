use super::*;

const MOTIFS: [Motif; 3] = [Motif::Veena, Motif::Loom, Motif::Chisel];

fn suite_total() -> u64 {
    SUITE.iter().map(span).sum()
}

#[test]
fn both_tiers_are_single_width_on_any_frame() {
    // The counter wraps, and a double-width glyph would shift the status line.
    for frame in [0, 1, 5, 6, 23, 24, 601, u64::MAX] {
        assert_eq!(primary_at(frame).0.chars().count(), 1);
        assert_eq!(secondary(frame).chars().count(), 1);
    }
    for m in &SUITE {
        for g in m.frames {
            assert_eq!(g.chars().count(), 1, "{g:?} is not one char");
        }
    }
}

#[test]
fn the_ambient_tier_runs_slower_than_the_spinner() {
    // A background indicator moving at the same rate would compete with the one
    // the user is actually waiting on.
    let primary_changes = (0..600)
        .filter(|f| primary_at(*f).0 != primary_at(f + 1).0)
        .count();
    let secondary_changes = (0..600)
        .filter(|f| secondary(*f) != secondary(f + 1))
        .count();
    assert!(
        secondary_changes < primary_changes,
        "secondary {secondary_changes} should change less often than primary {primary_changes}"
    );
}

#[test]
fn the_ambient_tier_shares_no_glyph_with_the_suite() {
    // The two are shown side by side; a shared glyph makes foreground and
    // background indistinguishable at a glance.
    for g in AMBIENT {
        for m in &SUITE {
            assert!(!m.frames.contains(&g), "{g:?} appears in both tiers");
        }
    }
}

#[test]
fn the_suite_plays_every_movement_before_repeating() {
    // A suite that never reaches its later movements is just a loop with dead
    // code behind it.
    let total = suite_total();
    for m in &SUITE {
        let first = m.frames[0];
        assert!(
            (0..total).any(|f| primary_at(f).0 == first),
            "a movement starting {first:?} never plays"
        );
    }
}

#[test]
fn every_movement_is_paced_for_legibility() {
    // anim_frame advances every 16ms. Undivided, a spinner cycles ~60 times a
    // second and smears; the arc set medha shipped ran 6x its designed speed.
    for m in &SUITE {
        let ms = m.divisor * 16;
        assert!(
            (48..=160).contains(&ms),
            "a movement runs at {ms}ms per frame, outside the legible range"
        );
    }
    let ambient_ms = AMBIENT_DIVISOR * 16;
    assert!(
        ambient_ms >= 160,
        "the ambient tier at {ambient_ms}ms is not calm"
    );
}

#[test]
fn brightness_swells_and_never_leaves_the_ramp() {
    for frame in 0..suite_total() {
        let (_, lit) = primary_at(frame);
        assert!(lit <= 100, "lit {lit} is off the ramp");
        assert!(lit >= MIN_LIT, "lit {lit} is below the floor");
    }
    // Flat brightness would be a glyph swap, not a twinkle.
    let levels: std::collections::BTreeSet<u16> =
        (0..suite_total()).map(|f| primary_at(f).1).collect();
    assert!(levels.len() > 1, "the spinner never changes brightness");
}

#[test]
fn every_motif_is_single_width_so_the_layout_never_shifts() {
    for motif in MOTIFS {
        let t = track(motif);
        for g in t.glyphs.iter().chain(std::iter::once(&t.head_glyph)) {
            assert_eq!(
                g.chars().count(),
                1,
                "{motif:?} glyph {g:?} is not one char"
            );
        }
    }
}

#[test]
fn the_head_replaces_a_glyph_rather_than_only_recolouring_it() {
    // A head that merely brightens the glyph under it reads as a dot sliding
    // along a bar — the same vague gesture for every theme.
    for motif in MOTIFS {
        let t = track(motif);
        let frame = (0..1000).find(|f| t.head(*f).is_some()).unwrap();
        let at = t.head(frame).unwrap();
        assert_eq!(t.glyph_at(at, frame), t.head_glyph);
        assert!(
            !t.glyphs.contains(&t.head_glyph),
            "{motif:?} head is already part of its ornament"
        );
    }
}

#[test]
fn every_motif_head_stays_in_bounds_on_any_frame() {
    for motif in MOTIFS {
        let t = track(motif);
        for frame in [0, 1, 7, 60, 601, u64::MAX] {
            if let Some(head) = t.head(frame) {
                assert!(head < t.glyphs.len(), "{motif:?} head {head} out of bounds");
            }
        }
    }
}

#[test]
fn every_motif_both_travels_and_rests() {
    // A track that never gaps reads as a loop with no pluck; one that never
    // shows a head is invisible.
    for motif in MOTIFS {
        let t = track(motif);
        let span = ((t.glyphs.len() + t.gap) as u64) * t.divisor;
        let heads = (0..span).filter_map(|f| t.head(f)).count();
        assert!(heads > 0, "{motif:?} never shows a highlight");
        assert!(heads < span as usize, "{motif:?} never rests");
    }
}

#[test]
fn the_motifs_are_visually_distinct() {
    let shapes: Vec<&[&str]> = MOTIFS.iter().map(|m| track(*m).glyphs).collect();
    for (i, a) in shapes.iter().enumerate() {
        for b in shapes.iter().skip(i + 1) {
            assert_ne!(a, b, "two motifs draw the same ornament");
        }
    }
    let heads: Vec<&str> = MOTIFS.iter().map(|m| track(*m).head_glyph).collect();
    for (i, a) in heads.iter().enumerate() {
        assert!(
            !heads.iter().skip(i + 1).any(|b| b == a),
            "two motifs share the head glyph {a}"
        );
    }
}

#[test]
fn each_motif_keeps_its_own_pace() {
    // Same cadence for all three would make the theme change invisible in
    // motion, which is half of what distinguishes them.
    let paces: Vec<(u64, usize)> = MOTIFS
        .iter()
        .map(|m| (track(*m).divisor, track(*m).gap))
        .collect();
    for (i, a) in paces.iter().enumerate() {
        assert!(
            !paces.iter().skip(i + 1).any(|b| b == a),
            "two motifs animate identically at {a:?}"
        );
    }
}
