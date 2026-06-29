//! Shared upstream-amount → integer-cents conversion with honest-failure guards.
//!
//! Providers receive monetary values as JSON floats (dollars). Converting to
//! `Cents` with a bare `(v * 100.0).round() as i64` silently saturates on
//! overflow and turns NaN/Inf into `0`/`i64::MAX`. Per the design spec (honest
//! failure, no silent fallbacks) a garbage upstream number must error, not
//! become a real-looking balance.

use arca_core::error::{CoreError, Result};
use arca_core::money::Cents;

/// Largest magnitude (in cents) an f64 represents without losing integer
/// precision (2^53). Past this the float→i64 cast silently drops low-order
/// digits before it even saturates.
const MAX_CENTS_F64: f64 = 9_007_199_254_740_992.0;

/// Convert an upstream dollar amount to integer cents, rejecting non-finite or
/// out-of-range values rather than silently saturating/truncating to a
/// plausible-but-wrong number. `ctx` names the call site for the error.
pub fn dollars_to_cents(ctx: &str, dollars: f64) -> Result<Cents> {
    let scaled = dollars * 100.0;
    if !scaled.is_finite() || scaled.abs() > MAX_CENTS_F64 {
        return Err(CoreError::Rpc(format!(
            "{ctx}: implausible amount {dollars}"
        )));
    }
    Ok(Cents(scaled.round() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_normal_dollars() {
        assert_eq!(dollars_to_cents("t", 20.00).unwrap(), Cents(2000));
        assert_eq!(dollars_to_cents("t", -19.99).unwrap(), Cents(-1999));
        assert_eq!(dollars_to_cents("t", 0.0).unwrap(), Cents(0));
    }

    #[test]
    fn rejects_non_finite_and_out_of_range() {
        assert!(dollars_to_cents("t", f64::NAN).is_err());
        assert!(dollars_to_cents("t", f64::INFINITY).is_err());
        assert!(dollars_to_cents("t", f64::NEG_INFINITY).is_err());
        assert!(dollars_to_cents("t", 1e18).is_err());
    }
}
