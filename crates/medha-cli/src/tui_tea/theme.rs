//! Colour themes, in one place.
//!
//! A theme is a whole visual identity, not just a set of text colours: the
//! canvas, the semantic slots, the tool-category hues, the splash wordmark and
//! the activity motif all come from the same [`Palette`]. Nothing theme-shaped
//! lives in `view.rs` — a new theme is one `const fn` here and nothing else.
//!
//! Colours are read through the accessor fns (`theme::text()` …), which return
//! slots from the *current* palette, so a `/theme` switch re-colours the whole
//! UI without threading a palette through every render fn. Reads take an
//! uncontended `RwLock` read (tens of ns) — negligible at a few hundred a frame.
//!
//! Every text slot clears 4.5:1 against the surface it is drawn on, and every
//! chrome slot 3:1. The one deliberate exception is `light.border` at 2.73: a
//! true 3:1 outline on parchment reads as a heavy black box.

use ratatui::style::Color;
use std::sync::RwLock;

use super::termbg;

#[cfg(test)]
#[path = "theme_tests.rs"]
mod tests;

pub type Rgb = (u8, u8, u8);

/// Which activity animation a theme draws. The shapes live in [`super::spin`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motif {
    /// Saraswati's veena, plucked — MEDHA's own instrument.
    Veena,
    /// A shuttle crossing the warp, for the indigo dyer's cloth.
    Loom,
    /// A stylus cutting a copper plate.
    Chisel,
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub id: &'static str,
    pub label: &'static str,
    pub is_dark: bool,
    pub motif: Motif,
    pub syntect_theme: &'static str,

    /// Canvas. `Color::Reset` keeps the terminal's own background, so a
    /// translucent terminal keeps its transparency; any other value paints over
    /// it. Only `dark` on a dark terminal can afford `Reset`.
    pub bg: Color,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub ok: Color,
    pub err: Color,
    pub warn: Color,
    pub lineno: Color,
    pub border: Color,
    pub link: Color,
    pub quote: Color,

    pub code_fg: Color,
    pub code_bg: Color,
    pub add_bg: Color,
    pub del_bg: Color,
    pub add_fg: Color,
    pub del_fg: Color,

    pub cat_read: Color,
    pub cat_search: Color,
    pub cat_web: Color,
    pub cat_vcs: Color,

    /// The lit-to-unlit ramp the spinner and the splash comet interpolate along,
    /// so each theme glows toward its own gold rather than toward white.
    pub comet_head: Rgb,
    pub comet_tail: Rgb,

    /// Splash wordmark, lit from the top: crown row first, base row last.
    pub logo: [Rgb; 6],
    /// The `medhā · intellect` line breathes between these two.
    pub word_lo: Rgb,
    pub word_hi: Rgb,
}

impl Palette {
    /// Canvas for a dark theme that has to supply its own — warm ink, matching
    /// `dark`'s `code_bg`.
    pub const DARK_CANVAS: Color = Color::Rgb(26, 24, 21);

    pub const fn dark() -> Self {
        Self {
            id: "dark",
            label: "Dark — intellect-gold on warm ink",
            is_dark: true,
            motif: Motif::Veena,
            syntect_theme: "base16-ocean.dark",
            bg: Color::Reset,
            text: Color::Rgb(230, 226, 216),
            dim: Color::Rgb(150, 142, 126),
            faint: Color::Rgb(124, 116, 101),
            accent: Color::Rgb(233, 181, 92),
            ok: Color::Rgb(150, 196, 128),
            err: Color::Rgb(226, 120, 100),
            warn: Color::Rgb(228, 178, 98),
            lineno: Color::Rgb(114, 106, 92),
            border: Color::Rgb(110, 101, 87),
            link: Color::Rgb(240, 206, 138),
            quote: Color::Rgb(172, 158, 134),
            code_fg: Color::Rgb(224, 200, 148),
            code_bg: Color::Rgb(34, 31, 27),
            add_bg: Color::Rgb(26, 44, 30),
            del_bg: Color::Rgb(52, 28, 26),
            add_fg: Color::Rgb(158, 210, 150),
            del_fg: Color::Rgb(232, 140, 128),
            cat_read: Color::Rgb(150, 178, 236),
            cat_search: Color::Rgb(196, 160, 240),
            cat_web: Color::Rgb(120, 200, 210),
            cat_vcs: Color::Rgb(230, 152, 102),
            comet_head: (255, 246, 214),
            comet_tail: (150, 120, 70),
            logo: [
                (255, 248, 224),
                (247, 208, 120),
                (230, 176, 84),
                (206, 150, 78),
                (176, 126, 66),
                (150, 108, 56),
            ],
            word_lo: (214, 158, 74),
            word_hi: (255, 248, 224),
        }
    }

    /// The dark palette, told whether the terminal underneath it is light.
    /// Forcing dark onto a light terminal without painting a canvas leaves
    /// near-white text on white.
    pub const fn dark_on(light_terminal: bool) -> Self {
        let mut p = Self::dark();
        if light_terminal {
            p.bg = Self::DARK_CANVAS;
        }
        p
    }

    pub const fn light() -> Self {
        Self {
            id: "light",
            label: "Light — ink on parchment",
            is_dark: false,
            motif: Motif::Veena,
            syntect_theme: "InspiredGitHub",
            bg: Color::Rgb(249, 246, 239),
            text: Color::Rgb(43, 38, 30),
            dim: Color::Rgb(106, 97, 83),
            faint: Color::Rgb(138, 129, 114),
            accent: Color::Rgb(140, 92, 14),
            ok: Color::Rgb(46, 108, 52),
            err: Color::Rgb(178, 50, 36),
            warn: Color::Rgb(142, 94, 12),
            lineno: Color::Rgb(144, 135, 120),
            border: Color::Rgb(160, 149, 131),
            link: Color::Rgb(132, 84, 16),
            quote: Color::Rgb(106, 97, 83),
            code_fg: Color::Rgb(112, 74, 12),
            code_bg: Color::Rgb(243, 237, 224),
            add_bg: Color::Rgb(224, 244, 226),
            del_bg: Color::Rgb(250, 226, 222),
            add_fg: Color::Rgb(30, 98, 46),
            del_fg: Color::Rgb(166, 40, 32),
            cat_read: Color::Rgb(38, 92, 168),
            cat_search: Color::Rgb(108, 62, 170),
            cat_web: Color::Rgb(22, 112, 124),
            cat_vcs: Color::Rgb(158, 82, 26),
            comet_head: (92, 56, 10),
            comet_tail: (164, 136, 92),
            logo: [
                (198, 142, 50),
                (178, 120, 34),
                (158, 102, 24),
                (138, 86, 18),
                (120, 72, 14),
                (104, 62, 12),
            ],
            word_lo: (140, 92, 14),
            word_hi: (184, 128, 44),
        }
    }

    /// **nīla** — resist-dyed indigo with gold zari. Indigo is the dye India
    /// gave its name to, and the identity `mod.rs` has always claimed ("amber
    /// accent + indigo depth") without any palette ever having it.
    pub const fn indigo() -> Self {
        Self {
            id: "indigo",
            label: "Indigo — nīla, gold on resist-dyed cloth",
            is_dark: true,
            motif: Motif::Loom,
            syntect_theme: "base16-ocean.dark",
            bg: Color::Rgb(22, 26, 45),
            text: Color::Rgb(226, 228, 236),
            dim: Color::Rgb(148, 154, 176),
            faint: Color::Rgb(112, 120, 146),
            accent: Color::Rgb(238, 190, 106),
            ok: Color::Rgb(126, 196, 168),
            err: Color::Rgb(228, 116, 116),
            warn: Color::Rgb(232, 178, 110),
            lineno: Color::Rgb(102, 110, 136),
            border: Color::Rgb(94, 103, 137),
            link: Color::Rgb(142, 186, 238),
            quote: Color::Rgb(160, 168, 192),
            code_fg: Color::Rgb(222, 206, 158),
            code_bg: Color::Rgb(29, 34, 56),
            add_bg: Color::Rgb(24, 48, 44),
            del_bg: Color::Rgb(58, 30, 46),
            add_fg: Color::Rgb(140, 208, 168),
            del_fg: Color::Rgb(234, 142, 152),
            cat_read: Color::Rgb(138, 176, 242),
            cat_search: Color::Rgb(190, 160, 240),
            cat_web: Color::Rgb(120, 200, 214),
            cat_vcs: Color::Rgb(232, 154, 110),
            comet_head: (244, 240, 255),
            comet_tail: (112, 124, 172),
            logo: [
                (248, 244, 232),
                (238, 200, 124),
                (220, 176, 96),
                (186, 152, 96),
                (140, 126, 116),
                (104, 102, 118),
            ],
            word_lo: (200, 158, 90),
            word_hi: (250, 238, 206),
        }
    }

    /// **tāmrapatra** — the engraved copper plate that carried royal grants for
    /// a millennium: oxidised ground, bright copper cut, verdigris in the
    /// recesses.
    pub const fn copper() -> Self {
        Self {
            id: "copper",
            label: "Copper — tāmrapatra, engraved plate and verdigris",
            is_dark: true,
            motif: Motif::Chisel,
            syntect_theme: "base16-mocha.dark",
            bg: Color::Rgb(22, 26, 24),
            text: Color::Rgb(232, 222, 210),
            dim: Color::Rgb(154, 146, 134),
            faint: Color::Rgb(120, 116, 106),
            accent: Color::Rgb(216, 142, 76),
            ok: Color::Rgb(94, 182, 156),
            err: Color::Rgb(228, 118, 98),
            warn: Color::Rgb(224, 168, 92),
            lineno: Color::Rgb(112, 108, 100),
            border: Color::Rgb(100, 110, 102),
            link: Color::Rgb(120, 198, 174),
            quote: Color::Rgb(162, 156, 144),
            code_fg: Color::Rgb(216, 184, 146),
            code_bg: Color::Rgb(29, 34, 31),
            add_bg: Color::Rgb(24, 46, 38),
            del_bg: Color::Rgb(52, 30, 28),
            add_fg: Color::Rgb(132, 204, 160),
            del_fg: Color::Rgb(234, 140, 122),
            cat_read: Color::Rgb(132, 180, 220),
            cat_search: Color::Rgb(184, 162, 226),
            cat_web: Color::Rgb(108, 196, 190),
            cat_vcs: Color::Rgb(226, 150, 96),
            comet_head: (248, 236, 222),
            comet_tail: (118, 128, 118),
            logo: [
                (250, 238, 224),
                (232, 176, 120),
                (216, 142, 76),
                (184, 120, 66),
                (146, 104, 66),
                (110, 92, 64),
            ],
            word_lo: (182, 116, 62),
            word_hi: (250, 232, 212),
        }
    }

    /// Every palette a `/theme <id>` can name. `auto` is a selector, not a
    /// palette, so it is absent here and resolved by [`resolve`].
    pub const ALL: [fn() -> Self; 4] = [Self::dark, Self::light, Self::indigo, Self::copper];

    /// The spinner's colour at brightness `lit` (0..=100): the comet tail lifted
    /// toward its head. Each theme therefore glows toward its own gold rather
    /// than every theme washing out to the same white.
    pub fn glow(&self, lit: u16) -> Color {
        let t = lit.min(100) as i32;
        let mix = |a: u8, b: u8| (a as i32 + (b as i32 - a as i32) * t / 100) as u8;
        Color::Rgb(
            mix(self.comet_tail.0, self.comet_head.0),
            mix(self.comet_tail.1, self.comet_head.1),
            mix(self.comet_tail.2, self.comet_head.2),
        )
    }
}

/// Rows for the `/theme` picker: the concrete palettes, then `auto`.
pub const AUTO: (&str, &str) = ("auto", "Auto — match the terminal background");

pub fn modes() -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<_> = Palette::ALL.iter().map(|f| (f().id, f().label)).collect();
    v.push(AUTO);
    v
}

/// What a session lands on when nothing else decides: no `MEDHA_THEME`, no
/// `/theme`, and a terminal that is dark or would not say.
///
/// Unlike `dark`, this paints an explicit canvas, so a translucent terminal
/// loses its transparency by default — `/theme dark` is the way back.
pub const fn default_palette() -> Palette {
    Palette::copper()
}

/// Resolve a `/theme` id — including `auto`, which reads what startup learned
/// rather than re-querying: by the time this runs, fd 1/2 are redirected to the
/// stray-stdout log and the terminal would never see the request.
pub fn resolve(id: &str) -> Palette {
    match id {
        "dark" => dark_for_terminal(),
        "light" => Palette::light(),
        "indigo" => Palette::indigo(),
        "copper" => Palette::copper(),
        "auto" => {
            if terminal_is_light() {
                Palette::light()
            } else {
                default_palette()
            }
        }
        _ => default_palette(),
    }
}

static CURRENT: RwLock<Palette> = RwLock::new(default_palette());

/// What the terminal reported at startup, kept because `/theme dark` needs it
/// long after the OSC query is gone. `None` when the terminal would not say.
static TERMINAL_IS_LIGHT: RwLock<Option<bool>> = RwLock::new(None);

/// `true` only when the terminal positively reported a light canvas; an unknown
/// terminal is treated as dark, which is the safe assumption for a dark palette.
pub fn terminal_is_light() -> bool {
    TERMINAL_IS_LIGHT.read().unwrap().unwrap_or(false)
}

pub fn dark_for_terminal() -> Palette {
    Palette::dark_on(terminal_is_light())
}

pub fn set(p: Palette) {
    *CURRENT.write().unwrap() = p;
}

/// Snapshot the whole palette — use when reading several slots at once (e.g. the
/// markdown renderer) to take one lock instead of many.
pub fn current() -> Palette {
    *CURRENT.read().unwrap()
}

/// Pick a palette at startup: `MEDHA_THEME` → OSC 11 → `COLORFGBG` →
/// [`default_palette`].
///
/// Must run BEFORE `tty::init`, which redirects fd 1/2 — after that the query
/// would be written to a file rather than the terminal.
pub fn detect() -> Palette {
    // Probe even when MEDHA_THEME forces a palette: `/theme` can switch away
    // later, and the answer is only available here.
    let reported_light = termbg::is_light();
    if let Some(light) = reported_light {
        *TERMINAL_IS_LIGHT.write().unwrap() = Some(light);
    }

    if let Ok(v) = std::env::var("MEDHA_THEME") {
        let id = v.trim().to_ascii_lowercase();
        if Palette::ALL.iter().any(|f| f().id == id) {
            return resolve(&id);
        }
    }
    match reported_light {
        Some(true) => Palette::light(),
        _ => default_palette(),
    }
}

pub fn bg() -> Color {
    CURRENT.read().unwrap().bg
}
pub fn accent() -> Color {
    CURRENT.read().unwrap().accent
}
pub fn text() -> Color {
    CURRENT.read().unwrap().text
}
pub fn dim() -> Color {
    CURRENT.read().unwrap().dim
}
pub fn faint() -> Color {
    CURRENT.read().unwrap().faint
}
pub fn ok() -> Color {
    CURRENT.read().unwrap().ok
}
pub fn err() -> Color {
    CURRENT.read().unwrap().err
}
pub fn warn() -> Color {
    CURRENT.read().unwrap().warn
}
pub fn lineno() -> Color {
    CURRENT.read().unwrap().lineno
}
pub fn add_bg() -> Color {
    CURRENT.read().unwrap().add_bg
}
pub fn del_bg() -> Color {
    CURRENT.read().unwrap().del_bg
}
pub fn add_fg() -> Color {
    CURRENT.read().unwrap().add_fg
}
pub fn del_fg() -> Color {
    CURRENT.read().unwrap().del_fg
}
pub fn code_fg() -> Color {
    CURRENT.read().unwrap().code_fg
}
pub fn code_bg() -> Color {
    CURRENT.read().unwrap().code_bg
}
pub fn border() -> Color {
    CURRENT.read().unwrap().border
}
pub fn link() -> Color {
    CURRENT.read().unwrap().link
}
pub fn quote() -> Color {
    CURRENT.read().unwrap().quote
}
