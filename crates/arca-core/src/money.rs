use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Integer cents. Money math never uses floats.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Cents(pub i64);

impl Cents {
    pub const ZERO: Self = Self(0);

    /// Parse a dollar string like `"-1234.56"`, `"$1,234.56"`, `"1234"`.
    pub fn from_dollars_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let (sign, rest) = match s.strip_prefix('-') {
            Some(r) => (-1i64, r),
            None => (1, s.strip_prefix('+').unwrap_or(s)),
        };
        let rest = rest.trim_start_matches('$').replace(',', "");
        let (whole, frac) = match rest.split_once('.') {
            Some((w, f)) => (w, f),
            None => (rest.as_str(), "0"),
        };
        if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
            return Err(CoreError::InvalidMoney(s.to_string()));
        }
        if frac.len() > 2 || !frac.chars().all(|c| c.is_ascii_digit()) {
            return Err(CoreError::InvalidMoney(s.to_string()));
        }
        let whole_cents: i64 = whole
            .parse::<i64>()
            .map_err(|_| CoreError::InvalidMoney(s.to_string()))?
            .checked_mul(100)
            .ok_or_else(|| CoreError::InvalidMoney(s.to_string()))?;
        let frac_cents: i64 = match frac.len() {
            0 => 0,
            1 => {
                frac.parse::<i64>()
                    .map_err(|_| CoreError::InvalidMoney(s.to_string()))?
                    * 10
            }
            2 => frac
                .parse::<i64>()
                .map_err(|_| CoreError::InvalidMoney(s.to_string()))?,
            _ => unreachable!(),
        };
        let total = whole_cents
            .checked_add(frac_cents)
            .and_then(|v| v.checked_mul(sign))
            .ok_or_else(|| CoreError::InvalidMoney(s.to_string()))?;
        Ok(Self(total))
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Cents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.0 < 0;
        let abs = self.0.unsigned_abs();
        let dollars = abs / 100;
        let cents = abs % 100;
        // Group dollars with commas.
        let mut buf = String::new();
        let s = dollars.to_string();
        for (i, ch) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                buf.push(',');
            }
            buf.push(ch);
        }
        let grouped: String = buf.chars().rev().collect();
        write!(f, "{}${}.{:02}", if neg { "-" } else { "" }, grouped, cents)
    }
}

impl Add for Cents {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Cents {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl Neg for Cents {
    type Output = Self;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl AddAssign for Cents {
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl SubAssign for Cents {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_sub(rhs.0);
    }
}

impl std::iter::Sum for Cents {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, |a, b| a + b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        assert_eq!(Cents::from_dollars_str("123.45").unwrap(), Cents(12_345));
        assert_eq!(Cents::from_dollars_str("0").unwrap(), Cents(0));
        assert_eq!(Cents::from_dollars_str("0.5").unwrap(), Cents(50));
        assert_eq!(Cents::from_dollars_str("-1.99").unwrap(), Cents(-199));
        assert_eq!(
            Cents::from_dollars_str("$1,234.56").unwrap(),
            Cents(123_456)
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(Cents::from_dollars_str("").is_err());
        assert!(Cents::from_dollars_str("abc").is_err());
        assert!(Cents::from_dollars_str("1.234").is_err());
        assert!(Cents::from_dollars_str("1.a").is_err());
    }

    #[test]
    fn display_formats() {
        assert_eq!(Cents(0).to_string(), "$0.00");
        assert_eq!(Cents(123_456).to_string(), "$1,234.56");
        assert_eq!(Cents(-50).to_string(), "-$0.50");
        assert_eq!(Cents(1_000_000_00).to_string(), "$1,000,000.00");
    }

    #[test]
    fn arithmetic() {
        let a = Cents(100);
        let b = Cents(250);
        assert_eq!(a + b, Cents(350));
        assert_eq!(b - a, Cents(150));
        assert_eq!(-a, Cents(-100));
        let sum: Cents = [a, b, Cents(50)].into_iter().sum();
        assert_eq!(sum, Cents(400));
    }

    #[test]
    fn parse_trailing_dot_yields_empty_fraction() {
        // A trailing dot leaves an empty fraction (frac.len() == 0) — the lenient
        // zero-cents branch, distinct from the no-dot path.
        assert_eq!(Cents::from_dollars_str("1.").unwrap(), Cents(100));
        assert_eq!(Cents::from_dollars_str("-5.").unwrap(), Cents(-500));
    }

    #[test]
    fn sub_assign_subtracts_in_place() {
        let mut c = Cents(100);
        c -= Cents(30);
        assert_eq!(c, Cents(70));
    }
}
