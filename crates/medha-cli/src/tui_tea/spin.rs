//! Activity animation, in one place.
//!
//! [`primary_at`] marks foreground work, [`secondary`] marks concurrent work at
//! a slower pace, and the wider splash [`track`] follows the theme's [`Motif`].
//!
//! Status-line motion is shared across themes; the palette supplies its colour.

use super::theme::Motif;

#[cfg(test)]
#[path = "spin_tests.rs"]
mod tests;

/// One movement of the spinner suite.
pub(super) struct Movement {
    pub frames: &'static [&'static str],
    /// Ticks per frame; `anim_frame` advances every 16 ms.
    pub divisor: u64,
}

const STAR_SMALL: [&str; 4] = ["✧", "✦", "✶", "✦"];
const TWINKLE: [&str; 6] = ["✶", "✸", "✹", "✺", "✹", "✷"];
const BLOOM: [&str; 6] = ["✳", "✼", "✻", "✽", "✻", "✼"];
const FLOWER: [&str; 4] = ["✿", "❀", "✾", "❀"];
const MOON: [&str; 8] = ["○", "◔", "◑", "◕", "●", "◕", "◑", "◔"];
const RINGS: [&str; 6] = ["◌", "◍", "◎", "◉", "◎", "◍"];

/// Six movements in sequence, each held [`CYCLES`] times.
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

/// Times each movement repeats before handing over.
const CYCLES: u64 = 2;

/// Dimmest the glyph goes mid-cycle, as a percentage of the lit colour. The
/// swell is what makes it twinkle rather than merely swap glyphs.
const MIN_LIT: u16 = 45;

/// A slow, quiet indicator for concurrent work.
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

/// A Saraswati veena: resonator, fretted neck, and upper gourd.
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

/// A themed ornament whose moving head replaces the underlying glyph.
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
        // A pluck followed by a long settle; the gourds stay lit.
        Motif::Veena => Track {
            glyphs: &VEENA,
            head_glyph: "◈",
            glow: &["◉", "○"],
            rim: &["◖", "◗"],
            gap: 8,
            divisor: 3,
        },
        Motif::Loom => Track {
            glyphs: &LOOM,
            head_glyph: "◆",
            glow: &[],
            rim: &["╞", "╡"],
            gap: 2,
            divisor: 2,
        },
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
