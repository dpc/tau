//! Fixed-point estimated equivalent API cost types.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProviderTokenUsage;

const MICRO_USD_PER_USD: u64 = 1_000_000;

/// An estimated USD price for one million tokens, stored in microdollars.
///
/// Serialization uses a decimal string so provider model declarations preserve
/// the configured value without binary floating-point conversion.
/// Deserialization also accepts integer JSON/CBOR numbers for ergonomic
/// provider configuration. Fractional prices must use strings so parsing never
/// passes through a binary floating-point representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EstimatedUsdPerMillion(u64);

impl EstimatedUsdPerMillion {
    /// Construct a whole-dollar price per million tokens when it fits.
    #[must_use]
    pub const fn checked_from_usd(usd: u64) -> Option<Self> {
        match usd.checked_mul(MICRO_USD_PER_USD) {
            Some(micro_usd) => Some(Self(micro_usd)),
            None => None,
        }
    }

    /// Construct a price from its fixed-point microdollar representation.
    #[must_use]
    pub const fn from_micro_usd(micro_usd: u64) -> Self {
        Self(micro_usd)
    }

    /// Return the fixed-point microdollar representation.
    #[must_use]
    pub const fn as_micro_usd(self) -> u64 {
        self.0
    }
}

impl std::str::FromStr for EstimatedUsdPerMillion {
    type Err = InvalidEstimatedUsdPerMillion;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = value
            .split_once('.')
            .map_or((value, None), |(whole, fraction)| (whole, Some(fraction)));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|fraction| {
                fraction.is_empty()
                    || fraction.len() > 6
                    || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(InvalidEstimatedUsdPerMillion);
        }
        let whole = whole
            .parse::<u64>()
            .ok()
            .and_then(|whole| whole.checked_mul(MICRO_USD_PER_USD))
            .ok_or(InvalidEstimatedUsdPerMillion)?;
        let fraction = fraction.unwrap_or_default();
        let fraction = if fraction.is_empty() {
            0
        } else {
            let parsed = fraction
                .parse::<u64>()
                .map_err(|_| InvalidEstimatedUsdPerMillion)?;
            parsed
                .checked_mul(10_u64.pow(6_u32.saturating_sub(fraction.len() as u32)))
                .ok_or(InvalidEstimatedUsdPerMillion)?
        };
        whole
            .checked_add(fraction)
            .map(Self)
            .ok_or(InvalidEstimatedUsdPerMillion)
    }
}

impl std::fmt::Display for EstimatedUsdPerMillion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let whole = self.0 / MICRO_USD_PER_USD;
        let fraction = self.0 % MICRO_USD_PER_USD;
        if fraction == 0 {
            return write!(formatter, "{whole}");
        }
        let mut fraction = format!("{fraction:06}");
        while fraction.ends_with('0') {
            fraction.pop();
        }
        write!(formatter, "{whole}.{fraction}")
    }
}

impl Serialize for EstimatedUsdPerMillion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EstimatedUsdPerMillion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PriceVisitor;

        impl<'de> Visitor<'de> for PriceVisitor {
            type Value = EstimatedUsdPerMillion;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "a non-negative decimal USD price with at most six fractional digits",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value
                    .checked_mul(MICRO_USD_PER_USD)
                    .map(EstimatedUsdPerMillion)
                    .ok_or_else(|| E::custom(InvalidEstimatedUsdPerMillion))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                u64::try_from(value)
                    .map_err(|_| E::custom("estimated USD price must not be negative"))
                    .and_then(|value| self.visit_u64(value))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let _ = value;
                Err(E::custom(
                    "fractional estimated USD prices must use a decimal string",
                ))
            }
        }

        deserializer.deserialize_any(PriceVisitor)
    }
}

/// Invalid fixed-point estimated USD price.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidEstimatedUsdPerMillion;

impl std::fmt::Display for InvalidEstimatedUsdPerMillion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "estimated USD price must be a non-negative decimal with at most six fractional digits",
        )
    }
}

impl std::error::Error for InvalidEstimatedUsdPerMillion {}

/// The three basic token prices used for an equivalent API cost estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EstimatedApiCostRates {
    /// Uncached input-token price per million tokens.
    pub uncached_input: EstimatedUsdPerMillion,
    /// Provider-reported cached input-token price per million tokens.
    pub cached_input: EstimatedUsdPerMillion,
    /// Output-token price per million tokens.
    pub output: EstimatedUsdPerMillion,
}

/// GPT-5.6-equivalent fallback used when model metadata omits explicit pricing.
pub const ESTIMATED_API_COST_FALLBACK: EstimatedApiCostRates = EstimatedApiCostRates {
    uncached_input: EstimatedUsdPerMillion::from_micro_usd(5_000_000),
    cached_input: EstimatedUsdPerMillion::from_micro_usd(500_000),
    output: EstimatedUsdPerMillion::from_micro_usd(30_000_000),
};

/// Runtime-only estimated equivalent API cost, stored in picodollars.
///
/// At this scale, multiplying a token count by a microdollar-per-million-token
/// rate produces picodollars directly. Operations saturate rather than
/// wrapping.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EstimatedApiCost(u64);

impl EstimatedApiCost {
    /// Construct a value from picodollars.
    #[must_use]
    pub const fn from_picodollars(picodollars: u64) -> Self {
        Self(picodollars)
    }

    /// Return the fixed-point picodollar representation.
    #[must_use]
    pub const fn as_picodollars(self) -> u64 {
        self.0
    }

    /// Add one provider usage record using the prices for the model that served
    /// it.
    ///
    /// Providers without cached-token detail report zero cached tokens, which
    /// conservatively prices all reported input as uncached. A malformed cached
    /// count larger than total input is capped at total input.
    pub fn add_usage(&mut self, usage: &ProviderTokenUsage, rates: EstimatedApiCostRates) {
        let increment = Self::for_usage(usage, rates);
        self.0 = self.0.saturating_add(increment.0);
    }

    /// Calculate the cost increment for one response-local usage record.
    #[must_use]
    pub fn for_usage(usage: &ProviderTokenUsage, rates: EstimatedApiCostRates) -> Self {
        let cached = usage.prompt_cached_tokens.min(usage.prompt_sent_tokens);
        let uncached = usage.prompt_sent_tokens.saturating_sub(cached);
        let increment = u128::from(uncached)
            .saturating_mul(u128::from(rates.uncached_input.as_micro_usd()))
            .saturating_add(
                u128::from(cached).saturating_mul(u128::from(rates.cached_input.as_micro_usd())),
            )
            .saturating_add(
                u128::from(usage.response_received_tokens)
                    .saturating_mul(u128::from(rates.output.as_micro_usd())),
            );
        Self(u64::try_from(increment).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests;
