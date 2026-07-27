use super::*;

fn rgb(c: Color) -> Rgb {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        other => panic!("expected an rgb slot, got {other:?}"),
    }
}

fn luminance((r, g, b): Rgb) -> f32 {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// A dark palette that keeps `Color::Reset` is measured against the canvas it
/// paints when it has to supply its own.
fn canvas(p: &Palette) -> Rgb {
    match p.bg {
        Color::Reset => rgb(Palette::DARK_CANVAS),
        other => rgb(other),
    }
}

#[test]
fn every_text_slot_clears_wcag_aa_on_the_surface_it_is_drawn_on() {
    for build in Palette::ALL {
        let p = build();
        let (bg, code, add, del) = (canvas(&p), rgb(p.code_bg), rgb(p.add_bg), rgb(p.del_bg));
        let cases: [(&str, Color, Rgb); 15] = [
            ("text", p.text, bg),
            ("dim", p.dim, bg),
            ("accent", p.accent, bg),
            ("ok", p.ok, bg),
            ("err", p.err, bg),
            ("warn", p.warn, bg),
            ("link", p.link, bg),
            ("quote", p.quote, bg),
            ("cat_read", p.cat_read, bg),
            ("cat_search", p.cat_search, bg),
            ("cat_web", p.cat_web, bg),
            ("cat_vcs", p.cat_vcs, bg),
            ("code_fg", p.code_fg, code),
            ("add_fg", p.add_fg, add),
            ("del_fg", p.del_fg, del),
        ];
        for (slot, fg, on) in cases {
            let r = contrast(rgb(fg), on);
            assert!(r >= 4.5, "{}.{slot} is {r:.2}:1, needs 4.5", p.id);
        }
    }
}

#[test]
fn chrome_slots_clear_the_non_text_threshold() {
    // `light.border` is the one deliberate shortfall: a true 3:1 outline on
    // parchment reads as a heavy black box.
    for build in Palette::ALL {
        let p = build();
        let bg = canvas(&p);
        let floor = if p.id == "light" { 2.7 } else { 3.0 };
        for (slot, fg, on) in [
            ("faint", p.faint, bg),
            ("border", p.border, bg),
            ("lineno", p.lineno, rgb(p.code_bg)),
        ] {
            let r = contrast(rgb(fg), on);
            assert!(r >= floor, "{}.{slot} is {r:.2}:1, needs {floor}", p.id);
        }
    }
}

#[test]
fn the_spinner_stays_legible_across_its_whole_glow_ramp() {
    // The spinner dims as it twinkles; the dimmest point still has to be read
    // against the canvas, and each theme must glow toward its own gold rather
    // than every theme washing out to the same white.
    for build in Palette::ALL {
        let p = build();
        let bg = canvas(&p);
        let floor = 3.0;
        for lit in [0u16, 45, 70, 100] {
            let r = contrast(rgb(p.glow(lit)), bg);
            assert!(
                r >= floor,
                "{}.glow({lit}) is {r:.2}:1, needs {floor}",
                p.id
            );
        }
        assert_eq!(
            p.glow(0),
            Color::Rgb(p.comet_tail.0, p.comet_tail.1, p.comet_tail.2)
        );
        assert_eq!(
            p.glow(100),
            Color::Rgb(p.comet_head.0, p.comet_head.1, p.comet_head.2)
        );
        // Clamped, not wrapped: a brightness above the ramp must not overflow.
        assert_eq!(p.glow(255), p.glow(100));
    }
}

#[test]
fn the_grey_ramp_stays_ordered_and_separated() {
    // `faint` and `lineno` were once literally the same colour in light, so the
    // recessive tiers collapsed into one.
    for build in Palette::ALL {
        let p = build();
        let bg = canvas(&p);
        let (t, d, f) = (
            contrast(rgb(p.text), bg),
            contrast(rgb(p.dim), bg),
            contrast(rgb(p.faint), bg),
        );
        assert!(t > d && d > f, "{}: ramp out of order {t} {d} {f}", p.id);
        assert!(
            contrast(rgb(p.text), rgb(p.dim)) > 1.5,
            "{}: text and dim are indistinguishable",
            p.id
        );
        assert!(
            contrast(rgb(p.faint), rgb(p.lineno)) > 1.05,
            "{}: faint and lineno are the same colour",
            p.id
        );
    }
}

#[test]
fn the_logo_gradient_runs_bright_crown_to_dark_base() {
    for build in Palette::ALL {
        let p = build();
        for (i, pair) in p.logo.windows(2).enumerate() {
            assert!(
                luminance(pair[0]) > luminance(pair[1]),
                "{}: logo row {i} is not brighter than {}",
                p.id,
                i + 1
            );
        }
        assert!(
            luminance(p.word_hi) > luminance(p.word_lo),
            "{}: the wordmark pulse does not brighten",
            p.id
        );
    }
}

#[test]
fn every_palette_has_a_distinct_id_and_resolves_back_to_itself() {
    let mut seen = Vec::new();
    for build in Palette::ALL {
        let p = build();
        assert!(!seen.contains(&p.id), "duplicate theme id {}", p.id);
        seen.push(p.id);
        assert_eq!(resolve(p.id).id, p.id);
    }
    // `auto` is a selector, not a palette, so it must never appear as one.
    assert!(!seen.contains(&AUTO.0));
    assert!(modes().iter().any(|(id, _)| *id == AUTO.0));
    assert_eq!(modes().len(), Palette::ALL.len() + 1);
}

#[test]
fn an_unknown_theme_id_falls_back_to_the_default_rather_than_panicking() {
    assert_eq!(resolve("chartreuse").id, default_palette().id);
    assert_eq!(resolve("").id, default_palette().id);
}

#[test]
fn every_theme_can_be_reached_and_left_again() {
    // Switching must be a closed loop: from any palette, naming any other id
    // lands exactly there, so no theme can become a one-way door.
    for from in Palette::ALL {
        for to in Palette::ALL {
            assert_eq!(resolve(to().id).id, to().id, "{} -> {}", from().id, to().id);
        }
        // …and the picker can always find the row for wherever you are.
        assert!(
            modes().iter().any(|(id, _)| *id == from().id),
            "{} is missing from the picker",
            from().id
        );
    }
}

#[test]
fn a_fresh_session_starts_on_the_default_palette() {
    // `CURRENT` is initialised before `detect` runs, so it and the fallback in
    // `detect` must not be able to drift apart.
    assert_eq!(default_palette().id, "copper");
    assert_eq!(current().id, default_palette().id);
    assert!(default_palette().is_dark);
    // Every id in the picker must still resolve to itself, default or not.
    assert_eq!(resolve("dark").id, "dark");
}

#[test]
fn only_dark_keeps_the_terminals_own_background() {
    // Every other palette paints its canvas; `dark` alone leaves `Reset` so a
    // translucent terminal keeps its transparency.
    assert_eq!(Palette::dark().bg, Color::Reset);
    assert_eq!(Palette::dark_on(true).bg, Palette::DARK_CANVAS);
    for build in [Palette::light, Palette::indigo, Palette::copper] {
        assert_ne!(
            build().bg,
            Color::Reset,
            "{} must paint a canvas",
            build().id
        );
    }
}

#[test]
fn each_theme_names_a_motif_and_the_signature_pair_shares_one() {
    assert_eq!(Palette::dark().motif, Motif::Veena);
    assert_eq!(Palette::light().motif, Motif::Veena);
    assert_eq!(Palette::indigo().motif, Motif::Loom);
    assert_eq!(Palette::copper().motif, Motif::Chisel);
}
