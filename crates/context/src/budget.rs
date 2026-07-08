//! Token budget derived from the model's context window (§4.3).
//!
//! usable = max_ctx − reserved_output − safety_buffer
//!
//! The window must reserve room for the model's own output plus a safety margin
//! before any input history is allotted. A *fixed* absolute reserve does not
//! scale: on a small open-weight window (e.g. 8k) a multi-tens-of-thousands
//! reserve goes negative and zeroes out usable context entirely. We therefore
//! reserve the *lesser* of an absolute cap or a fraction of the window, so small
//! windows stay workable while large windows still get generous headroom. This
//! keeps the harness model-agnostic across the full open-to-frontier range.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_ctx: u32,
    pub reserved_output: u32,
    pub safety_buffer: u32,
}

impl ContextBudget {
    /// Absolute caps (used for large windows). Below them we scale by fraction.
    pub const MAX_RESERVED_OUTPUT: u32 = 32_000;
    pub const MAX_SAFETY_BUFFER: u32 = 20_000;

    /// Derive a budget from a known context window, scaling reservations so a
    /// small (open-source) window is never reserved into the ground.
    pub fn from_max_ctx(max_ctx: u32) -> Self {
        let reserved_output = (max_ctx / 4).min(Self::MAX_RESERVED_OUTPUT); // ≤25% or 32k
        let safety_buffer = (max_ctx / 10).min(Self::MAX_SAFETY_BUFFER); //   ≤10% or 20k
        Self { max_ctx, reserved_output, safety_buffer }
    }

    /// Tokens available for input history after reservations.
    pub fn usable(&self) -> u32 {
        self.max_ctx
            .saturating_sub(self.reserved_output)
            .saturating_sub(self.safety_buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_window_uses_absolute_caps() {
        let b = ContextBudget::from_max_ctx(200_000);
        assert_eq!(b.reserved_output, 32_000);
        assert_eq!(b.safety_buffer, 20_000);
        assert_eq!(b.usable(), 148_000);
    }

    #[test]
    fn small_open_window_stays_usable() {
        // A fixed absolute reserve (tens of thousands) would drive usable
        // context negative here; fractional scaling must keep it workable.
        let b = ContextBudget::from_max_ctx(8_000);
        assert_eq!(b.reserved_output, 2_000); // 8000/4
        assert_eq!(b.safety_buffer, 800); //    8000/10
        assert_eq!(b.usable(), 5_200); //        still room to work
    }

    #[test]
    fn mid_window_scales_proportionally() {
        let b = ContextBudget::from_max_ctx(32_768);
        assert_eq!(b.reserved_output, 8_192);
        assert_eq!(b.safety_buffer, 3_276);
        assert!(b.usable() > 20_000);
    }
}
