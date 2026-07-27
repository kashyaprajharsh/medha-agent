//! Activity animation, in one place.
//!
//! Three tiers, so concurrent indicators do not compete: [`primary_at`] for
//! whatever the user is waiting on, [`secondary`] at a slower pace for work
//! running alongside, and [`track`] — the wide ornament on the splash, the one
//! place with room for a theme's own shape, so it alone follows [`Motif`].
//!
//! The two status-line tiers are shared across themes and differ by colour: a
//! palette supplies the ramp they glow along, so the same gesture reads as gold
//! on `dark` and as copper on `copper`.

use super::theme::Motif;

#[cfg(test)]
#[path = "spin_tests.rs"]
mod tests;

/// One movement of the spinner suite.
pub(super) struct Movement {
    pub frames: &'static [&'static str],
    /// Ticks per frame. `anim_frame` advances every 16 ms, so an undivided
    /// spinner cycles ~60 times a second — a smear, not an animation. These land
    /// in the 64–128 ms per frame a spinner stays legible at.
    pub divisor: u64,
}

const STAR_SMALL: [&str; 4] = ["✧", "✦", "✶", "✦"];
const TWINKLE: [&str; 6] = ["✶", "✸", "✹", "✺", "✹", "✷"];
const BLOOM: [&str; 6] = ["✳", "✼", "✻", "✽", "✻", "✼"];
const FLOWER: [&str; 4] = ["✿", "❀", "✾", "❀"];
const MOON: [&str; 8] = ["○", "◔", "◑", "◕", "●", "◕", "◑", "◔"];
const RINGS: [&str; 6] = ["◌", "◍", "◎", "◉", "◎", "◍"];

/// The spinner is a suite, not a loop: six movements in sequence, each held
/// [`CYCLES`] times before the next takes over. A single four-frame loop is what
/// makes most CLI spinners read as machinery — a progression stays alive over a
/// long turn without ever getting louder.
const SUITE: [Movement; 6] = [
    Movement {
        frames: &STAR_SMALL,
        divisor: 6,
    },
    Movement {
        frames: &TWINKLE,
        divisor: 4,
    },
    Movement {
        frames: &BLOOM,
        divisor: 5,
    },
    Movement {
        frames: &FLOWER,
        divisor: 8,
    },
    Movement {
        frames: &MOON,
        divisor: 5,
    },
    Movement {
        frames: &RINGS,
        divisor: 5,
    },
];

/// Times each movement repeats before handing over. One reads as restless; four
/// hides that the suite is progressing at all.
const CYCLES: u64 = 2;

/// Dimmest the glyph goes mid-cycle, as a percentage of the lit colour. The
/// swell is what makes it twinkle rather than merely swap glyphs.
const MIN_LIT: u16 = 45;

/// Work happening alongside: sub-agents, background tasks. One quiet set at a
/// slow pace, deliberately outside the suite — the ambient tier is never the
/// thing being waited on, and must not compete with it.
const AMBIENT: [&str; 4] = ["⋅", "·", "∘", "·"];
const AMBIENT_DIVISOR: u64 = 12;

fn span(m: &Movement) -> u64 {
    m.frames.len() as u64 * m.divisor * CYCLES
}

/// The spinner's glyph and how lit it is, 0..=100, at this frame.
pub(super) fn primary_at(frame: u64) -> (&'static str, u16) {
    let total: u64 = SUITE.iter().map(span).sum();
    let mut pos = frame % total.max(1);
    for m in &SUITE {
        let s = span(m);
        if pos < s {
            let n = m.frames.len();
            let idx = ((pos / m.divisor) as usize) % n;
            return (m.frames[idx], lit(idx, n));
        }
        pos -= s;
    }
    (SUITE[0].frames[0], 100)
}

/// Brightest at the ends of a cycle, dimmest at its midpoint.
fn lit(idx: usize, n: usize) -> u16 {
    let half = (n / 2) as u16;
    if half == 0 {
        return 100;
    }
    let d = if idx as u16 <= half {
        idx as u16
    } else {
        n as u16 - idx as u16
    };
    100 - d * (100 - MIN_LIT) / half
}

pub(super) fn secondary(frame: u64) -> &'static str {
    AMBIENT[((frame / AMBIENT_DIVISOR) as usize) % AMBIENT.len()]
}

/// A Saraswati veena — resonator gourd with soundhole, fretted neck, upper
/// gourd. Playing it means tuning the intellect into harmony, so the gesture is
/// a *pluck*: resonance runs the neck, then the string is left to settle.
const VEENA: [&str; 19] = [
    "◖", "◉", "◗", "─", "┼", "─", "┼", "─", "┼", "─", "┼", "─", "┼", "─", "┼", "─", "┼", "─", "○",
];

/// A loom warp under tension, for the indigo dyer's cloth: short, dense, and
/// crossed end to end without pause.
const LOOM: [&str; 13] = [
    "╞", "═", "╪", "═", "╪", "═", "╪", "═", "╪", "═", "╪", "═", "╡",
];

/// A copper plate scored for engraving: the dotted rule is the line not yet
/// cut, the ticks are where the stylus has already bitten.
const CHISEL: [&str; 15] = [
    "▫", "┄", "┴", "┄", "┴", "┄", "┴", "┄", "┴", "┄", "┴", "┄", "┴", "┄", "▪",
];

/// A themed ornament with a moving head. The head does not merely brighten a
/// glyph — it *replaces* it, so each motif reads as its own gesture rather than
/// as a dot sliding along a bar.
pub(super) struct Track {
    pub glyphs: &'static [&'static str],
    /// Drawn in place of `glyphs[head]` while the head is there.
    pub head_glyph: &'static str,
    /// Glyphs that shine whether or not the head is on them — the resonating
    /// bodies of the ornament.
    pub glow: &'static [&'static str],
    /// Glyphs that carry the accent: rims and edges.
    pub rim: &'static [&'static str],
    /// Frames of stillness after the head leaves, before it re-enters.
    pub gap: usize,
    /// Ticks each step holds; larger is slower.
    pub divisor: u64,
}

impl Track {
    /// Index of the head, or `None` during the rest.
    pub fn head(&self, frame: u64) -> Option<usize> {
        let span = self.glyphs.len() + self.gap;
        let at = (frame / self.divisor) as usize % span;
        (at < self.glyphs.len()).then_some(at)
    }

    /// The glyph to draw at `i` for this frame.
    pub fn glyph_at(&self, i: usize, frame: u64) -> &'static str {
        if self.head(frame) == Some(i) {
            self.head_glyph
        } else {
            self.glyphs[i]
        }
    }
}

/// Light box-drawing only. The heavy and mixed-weight forms (`━`, `┿`) and the
/// pegbox curl (`╮`) fall back to unrelated glyphs in common terminal fonts —
/// the curl rendered as a stray `⌐` hanging off the end.
pub(super) fn track(motif: Motif) -> Track {
    match motif {
        // A pluck, then a long settle before the next one. The gourds glow
        // throughout — Saraswati's white, the colour of true knowledge.
        Motif::Veena => Track {
            glyphs: &VEENA,
            head_glyph: "◈",
            glow: &["◉", "○"],
            rim: &["◖", "◗"],
            gap: 8,
            divisor: 3,
        },
        // The shuttle crosses and returns almost at once; weaving is continuous.
        Motif::Loom => Track {
            glyphs: &LOOM,
            head_glyph: "◆",
            glow: &[],
            rim: &["╞", "╡"],
            gap: 2,
            divisor: 2,
        },
        // One deliberate stroke at a time, with the hand lifted between.
        Motif::Chisel => Track {
            glyphs: &CHISEL,
            head_glyph: "▼",
            glow: &["▪"],
            rim: &["▫"],
            gap: 6,
            divisor: 5,
        },
    }
}
