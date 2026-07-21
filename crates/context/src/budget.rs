//! Input budget derived from resolved model limits and count quality (§4.3).
//!
//! Output reservation is handled by `ModelLimits::input_allowance`; this type
//! never invents a percentage of the total window. Only non-authoritative
//! preflight sources receive a documented safety margin.

use kernel::TokenCountQuality;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub input_limit: u32,
    pub safety_buffer: u32,
}

impl ContextBudget {
    const PROVIDER_ESTIMATE_MARGIN_BPS: u32 = 200; // 2%
    const LOCAL_ESTIMATE_MARGIN_BPS: u32 = 1_000; // 10%

    pub fn from_input_limit(input_limit: u32, quality: TokenCountQuality) -> Self {
        let basis_points = match quality {
            TokenCountQuality::Authoritative => 0,
            TokenCountQuality::ProviderEstimate => Self::PROVIDER_ESTIMATE_MARGIN_BPS,
            TokenCountQuality::LocalEstimate => Self::LOCAL_ESTIMATE_MARGIN_BPS,
        };
        let safety_buffer = input_limit.saturating_mul(basis_points) / 10_000;
        Self { input_limit, safety_buffer }
    }

    /// Backward-compatible shorthand for callers doing local estimation.
    pub fn from_max_ctx(input_limit: u32) -> Self {
        Self::from_input_limit(input_limit, TokenCountQuality::LocalEstimate)
    }

    /// Tokens available for input history after reservations.
    pub fn usable(&self) -> u32 {
        self.input_limit.saturating_sub(self.safety_buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_count_uses_the_full_resolved_input_allowance() {
        let b = ContextBudget::from_input_limit(200_000, TokenCountQuality::Authoritative);
        assert_eq!(b.safety_buffer, 0);
        assert_eq!(b.usable(), 200_000);
    }

    #[test]
    fn local_estimate_uses_a_source_specific_margin() {
        let b = ContextBudget::from_max_ctx(8_000);
        assert_eq!(b.safety_buffer, 800);
        assert_eq!(b.usable(), 7_200);
    }

    #[test]
    fn provider_estimate_has_a_smaller_margin_than_local_tokenization() {
        let provider =
            ContextBudget::from_input_limit(100_000, TokenCountQuality::ProviderEstimate);
        let local = ContextBudget::from_max_ctx(100_000);
        assert_eq!(provider.usable(), 98_000);
        assert_eq!(local.usable(), 90_000);
    }
}
