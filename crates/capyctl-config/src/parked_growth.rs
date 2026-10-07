//! ADR 0014 amendment A18 (owner decision 2026-10-07): how far one launch's
//! measured parked charge may grow past its first measured park before its
//! next park is a stop.
//!
//! Found on the 2026-10-06 catalog run (GB10, vLLM 0.30.0, Qwen3.8-27B NVFP4):
//! each wake left CPU-side memory behind, and the parked charge CapyCTL
//! measured grew from 4.7 to 9.6 and then 13.4 GiB over three parks of one
//! launch while the GPU memory returned to 22.1 GiB after every wake. The
//! accounting stayed honest (A13 measures every park), but the engine held
//! more of the machine each cycle.
//!
//! `resource_policy.parked_growth_limit` is host policy, beside `parked_limit`
//! and `max_parked`: the host's memory is what the growth takes, and the same
//! engine grows on one platform and not on another. It takes:
//!
//! - `auto` (the default): growth up to the first measured charge itself
//!   (100 %), so a launch may double its first parked charge;
//! - a whole percentage of the first measured charge (`50%`, `200%`);
//! - a size, the growth allowed in bytes (`8GiB`);
//! - `off`: never stop for growth.
//!
//! A policy that states nothing, or `auto`, publishes and stores exactly what
//! it did before the setting existed.

use serde::{Serialize, Serializer};

/// The bound on a launch's parked-charge growth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParkedGrowthLimit {
    /// [`AUTO_PERCENT`] of the first measured charge.
    #[default]
    Auto,
    /// Never stop for growth.
    Off,
    /// A whole percentage of the first measured charge.
    Percent(u32),
    /// The growth allowed, in bytes.
    Bytes(i64),
}

/// `auto`: growth up to the first measured charge.
pub const AUTO_PERCENT: u32 = 100;

/// The largest percentage accepted (a hundredfold growth).
pub const MAX_PERCENT: u32 = 10_000;

impl ParkedGrowthLimit {
    /// `auto`, `off`, `N%` (a whole percentage from 0 % to 10000 %) or a size.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "auto" => return Ok(Self::Auto),
            "off" => return Ok(Self::Off),
            _ => {}
        }
        if let Some(number) = text.strip_suffix('%') {
            return match number.parse::<u32>() {
                Ok(percent) if percent <= MAX_PERCENT && !number.starts_with('+') => {
                    Ok(Self::Percent(percent))
                }
                _ => Err(format!(
                    "`{text}` is not a whole percentage from 0% to {MAX_PERCENT}%"
                )),
            };
        }
        crate::effective::parse_bytes(text)
            .map(Self::Bytes)
            .map_err(|_| {
                format!(
                    "`{text}` is neither `auto`, `off`, a size such as `8GiB` nor a percentage \
                     such as `100%`"
                )
            })
    }

    /// The canonical text a document or a stored policy carries; `None` for
    /// `auto`, which is never written, so a policy without the setting keeps
    /// its bytes and digest.
    pub fn stated(self) -> Option<String> {
        match self {
            Self::Auto => None,
            Self::Off => Some("off".into()),
            Self::Percent(percent) => Some(format!("{percent}%")),
            Self::Bytes(bytes) => Some(format!("{bytes}B")),
        }
    }

    /// The growth past `first` (the first measured charge, in bytes) a
    /// launch may reach; a park is turned into a stop only above it. `None`
    /// when the limit is off.
    pub fn bound(self, first: i64) -> Option<i64> {
        let percent = |p: u32| {
            i64::try_from(i128::from(first.max(0)) * i128::from(p) / 100).unwrap_or(i64::MAX)
        };
        match self {
            Self::Auto => Some(percent(AUTO_PERCENT)),
            Self::Off => None,
            Self::Percent(p) => Some(percent(p)),
            Self::Bytes(bytes) => Some(bytes),
        }
    }

    /// Whether this is the default.
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

impl Serialize for ParkedGrowthLimit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.stated().as_deref().unwrap_or("auto"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T03 (ADR 0014 amendment A18): the forms the setting takes.
    #[test]
    fn the_growth_limit_takes_auto_off_a_percentage_or_a_size() {
        assert_eq!(
            ParkedGrowthLimit::parse("auto"),
            Ok(ParkedGrowthLimit::Auto)
        );
        assert_eq!(ParkedGrowthLimit::parse("off"), Ok(ParkedGrowthLimit::Off));
        assert_eq!(
            ParkedGrowthLimit::parse("50%"),
            Ok(ParkedGrowthLimit::Percent(50))
        );
        assert_eq!(
            ParkedGrowthLimit::parse("8GiB"),
            Ok(ParkedGrowthLimit::Bytes(8 << 30))
        );
        for bad in ["-1%", "+5%", "7.5%", "10001%", "lots", "-1GiB", "1 GiB", ""] {
            assert!(ParkedGrowthLimit::parse(bad).is_err(), "{bad}");
        }
        for limit in [
            ParkedGrowthLimit::Off,
            ParkedGrowthLimit::Percent(150),
            ParkedGrowthLimit::Bytes(8 << 30),
        ] {
            let text = limit.stated().unwrap();
            assert_eq!(ParkedGrowthLimit::parse(&text), Ok(limit), "{text}");
        }
        assert_eq!(ParkedGrowthLimit::Auto.stated(), None);
    }

    // T16 (ADR 0014 amendment A18): the catalog's growth, 4.7 GiB first and
    // 9.6 GiB on the second park, is past the default bound.
    #[test]
    fn the_default_bound_is_the_first_charge() {
        let (first, second): (i64, i64) = (4_700 << 20, 9_600 << 20);
        let bound = ParkedGrowthLimit::Auto.bound(first).unwrap();
        assert_eq!(bound, first);
        assert!(
            second - first > bound,
            "the catalog's second park is past it"
        );
        assert_eq!(ParkedGrowthLimit::Percent(50).bound(100), Some(50));
        assert_eq!(ParkedGrowthLimit::Bytes(7).bound(100), Some(7));
        assert_eq!(ParkedGrowthLimit::Off.bound(100), None);
    }
}
