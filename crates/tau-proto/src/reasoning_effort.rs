//! Portable reasoning intent and model-native effort selection.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Number of exact decimal millionths in the nominal portable intensity range.
pub const REASONING_INTENSITY_ONE: i32 = 1_000_000;

/// Signed exact-millionth reasoning intensity.
///
/// Configured absolute values are restricted to `0.0..=1.0`, while composed
/// runtime intent may use the complete `i32` range until model lowering clamps
/// it.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReasoningIntensity(i32);

impl ReasoningIntensity {
    /// Portable medium-like intensity.
    pub const MEDIUM: Self = Self(500_000);

    /// Creates an intensity from its exact signed-millionth representation.
    #[must_use]
    pub const fn from_millionths(millionths: i32) -> Self {
        Self(millionths)
    }

    /// Returns the exact signed-millionth representation.
    #[must_use]
    pub const fn millionths(self) -> i32 {
        self.0
    }

    /// Returns this intensity clamped to the nominal portable range.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self(self.0.clamp(0, REASONING_INTENSITY_ONE))
    }

    /// Applies a signed delta, saturating only at the machine representation.
    #[must_use]
    pub const fn saturating_add(self, delta: ReasoningIntensityDelta) -> Self {
        Self(self.0.saturating_add(delta.0))
    }

    /// Returns whether this value is in the configured absolute-value range.
    #[must_use]
    pub const fn is_nominal(self) -> bool {
        0 <= self.0 && self.0 <= REASONING_INTENSITY_ONE
    }
}

impl fmt::Display for ReasoningIntensity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_millionths(self.0, formatter)
    }
}

impl FromStr for ReasoningIntensity {
    type Err = ParseReasoningIntensityError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parse_millionths(input).map(Self)
    }
}

impl Serialize for ReasoningIntensity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReasoningIntensity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ReasoningIntensity;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an exact decimal with at most six fractional digits")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.to_string().parse().map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.to_string().parse().map_err(E::custom)
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if !value.is_finite() {
                    return Err(E::custom("reasoning intensity must be finite"));
                }
                value.to_string().parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Nonzero exact-millionth change to a portable reasoning intensity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReasoningIntensityDelta(i32);

impl ReasoningIntensityDelta {
    /// Creates a delta, rejecting zero.
    pub const fn new(millionths: i32) -> Option<Self> {
        if millionths == 0 {
            None
        } else {
            Some(Self(millionths))
        }
    }

    /// Returns the signed exact-millionth representation.
    #[must_use]
    pub const fn millionths(self) -> i32 {
        self.0
    }
}

impl fmt::Display for ReasoningIntensityDelta {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_millionths(self.0, formatter)
    }
}

impl FromStr for ReasoningIntensityDelta {
    type Err = ParseReasoningIntensityError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let value = parse_millionths(input)?;
        Self::new(value).ok_or(ParseReasoningIntensityError::ZeroDelta)
    }
}

impl Serialize for ReasoningIntensityDelta {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReasoningIntensityDelta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let intensity = ReasoningIntensity::deserialize(deserializer)?;
        Self::new(intensity.millionths()).ok_or_else(|| de::Error::custom("delta must be nonzero"))
    }
}

/// Portable user/config/runtime reasoning request.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ReasoningIntent {
    /// Let the provider choose its route default and omit an effort selector.
    #[default]
    ProviderDefault,
    /// Request disabled reasoning, falling back to the model's minimum if
    /// needed.
    Disabled,
    /// Request a portable numeric intensity.
    Intensity(ReasoningIntensity),
}

impl ReasoningIntent {
    /// Returns whether this is a legal absolute config/UI value.
    ///
    /// Sentinel intents are always legal. Numeric intent must be in the nominal
    /// portable range; only relative arithmetic may create out-of-range state.
    #[must_use]
    pub const fn is_nominal(self) -> bool {
        match self {
            Self::ProviderDefault | Self::Disabled => true,
            Self::Intensity(value) => value.is_nominal(),
        }
    }

    /// Applies a signed delta and turns sentinel intents into numeric intent.
    #[must_use]
    pub const fn adjust(self, delta: ReasoningIntensityDelta) -> Self {
        let seed = match self {
            Self::ProviderDefault => ReasoningIntensity::MEDIUM,
            Self::Disabled => ReasoningIntensity::from_millionths(0),
            Self::Intensity(value) => value,
        };
        Self::Intensity(seed.saturating_add(delta))
    }
}

impl From<crate::NativeReasoningEffort> for ReasoningIntent {
    fn from(level: crate::NativeReasoningEffort) -> Self {
        ReasoningSelection::native(level).requested
    }
}

impl fmt::Display for ReasoningIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderDefault => formatter.write_str("provider_default"),
            Self::Disabled => formatter.write_str("disabled"),
            Self::Intensity(value) => value.fmt(formatter),
        }
    }
}

impl FromStr for ReasoningIntent {
    type Err = ParseReasoningIntensityError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "provider_default" => Ok(Self::ProviderDefault),
            "disabled" => Ok(Self::Disabled),
            value => value.parse().map(Self::Intensity),
        }
    }
}

impl Serialize for ReasoningIntent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReasoningIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ReasoningIntent;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("provider_default, disabled, or an exact decimal intensity")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value
                    .to_string()
                    .parse()
                    .map(ReasoningIntent::Intensity)
                    .map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value
                    .to_string()
                    .parse()
                    .map(ReasoningIntent::Intensity)
                    .map_err(E::custom)
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if !value.is_finite() {
                    return Err(E::custom("reasoning intensity must be finite"));
                }
                value
                    .to_string()
                    .parse()
                    .map(ReasoningIntent::Intensity)
                    .map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// One cut point mapping portable intensity to a native effort.
///
/// Cut points at or below [`ReasoningIntensity::MEDIUM`] belong to this higher
/// band. Cut points above medium belong to the preceding lower band.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReasoningEffortBand {
    /// Cut point in the nominal range.
    pub from: ReasoningIntensity,
    /// Native level selected above this cut point, including it only at or
    /// below medium.
    pub level: crate::NativeReasoningEffort,
}

/// Provider/model reasoning-effort control and portable mapping.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReasoningEffortControl {
    /// The route has no selectable control or exact fixed native claim.
    #[default]
    Unsupported,
    /// The route has one known fixed level and sends no selector.
    Fixed {
        /// Known fixed native level.
        level: crate::NativeReasoningEffort,
    },
    /// The route accepts mapped native selectors.
    Mapped {
        /// Strictly increasing inward-owned cut-point bands.
        mapping: Vec<ReasoningEffortBand>,
    },
}

/// Exact model reasoning capability, including a documented provider default.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReasoningEffortCapability {
    /// Selectable, fixed, or unsupported route behavior.
    pub control: ReasoningEffortControl,
    /// Exact native result of omission when the provider documents it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_default: Option<crate::NativeReasoningEffort>,
}

impl ReasoningEffortCapability {
    /// Returns a standard mapping for an ordered set of distinct native levels.
    #[must_use]
    pub fn mapped(levels: impl IntoIterator<Item = crate::NativeReasoningEffort>) -> Self {
        let levels: Vec<_> = levels.into_iter().collect();
        if levels.is_empty() {
            return Self::default();
        }
        let has_minimal = levels.contains(&crate::NativeReasoningEffort::Minimal);
        Self {
            control: ReasoningEffortControl::Mapped {
                mapping: levels
                    .into_iter()
                    .enumerate()
                    .map(|(index, level)| ReasoningEffortBand {
                        from: ReasoningIntensity::from_millionths(if index == 0 {
                            0
                        } else {
                            match level {
                                crate::NativeReasoningEffort::None => 0,
                                crate::NativeReasoningEffort::Minimal => 100_000,
                                crate::NativeReasoningEffort::Low if has_minimal => 200_000,
                                crate::NativeReasoningEffort::Low => 100_000,
                                crate::NativeReasoningEffort::Medium => 350_000,
                                crate::NativeReasoningEffort::High => 650_000,
                                crate::NativeReasoningEffort::XHigh => 800_000,
                                crate::NativeReasoningEffort::Max => 900_000,
                            }
                        }),
                        level,
                    })
                    .collect(),
            },
            provider_default: None,
        }
    }

    /// Returns whether this capability has a structurally valid portable
    /// mapping.
    ///
    /// Raw provider declarations remain deserializable so the harness can
    /// reject only the malformed model entry and publish a diagnostic.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match &self.control {
            ReasoningEffortControl::Unsupported => self.provider_default.is_none(),
            ReasoningEffortControl::Fixed { level } => self.provider_default == Some(*level),
            ReasoningEffortControl::Mapped { mapping } => {
                let Some(first) = mapping.first() else {
                    return false;
                };
                first.from.millionths() == 0
                    && mapping.iter().all(|band| band.from.is_nominal())
                    && mapping
                        .iter()
                        .skip(1)
                        .all(|band| band.from.millionths() < REASONING_INTENSITY_ONE)
                    && mapping
                        .windows(2)
                        .all(|pair| pair[0].from < pair[1].from && pair[0].level < pair[1].level)
                    && self
                        .provider_default
                        .is_none_or(|level| mapping.iter().any(|band| band.level == level))
            }
        }
    }

    /// Lowers portable intent once against this exact capability.
    #[must_use]
    pub fn select(&self, requested: ReasoningIntent) -> EffectiveReasoningEffort {
        match requested {
            ReasoningIntent::ProviderDefault => match self.control {
                ReasoningEffortControl::Fixed { level } => {
                    EffectiveReasoningEffort::ProviderDefault(Some(level))
                }
                ReasoningEffortControl::Unsupported | ReasoningEffortControl::Mapped { .. } => {
                    EffectiveReasoningEffort::ProviderDefault(self.provider_default)
                }
            },
            _ => match &self.control {
                ReasoningEffortControl::Unsupported => EffectiveReasoningEffort::Unsupported,
                ReasoningEffortControl::Fixed { level } => EffectiveReasoningEffort::Fixed(*level),
                ReasoningEffortControl::Mapped { mapping } => {
                    let numeric = match requested {
                        ReasoningIntent::Disabled => {
                            if let Some(none) = mapping
                                .iter()
                                .find(|band| band.level == crate::NativeReasoningEffort::None)
                            {
                                return EffectiveReasoningEffort::Native(none.level);
                            }
                            ReasoningIntensity::from_millionths(0)
                        }
                        ReasoningIntent::Intensity(value) => value.clamped(),
                        ReasoningIntent::ProviderDefault => unreachable!(),
                    };
                    mapping
                        .iter()
                        .rev()
                        .find(|band| {
                            if band.from <= ReasoningIntensity::MEDIUM {
                                band.from <= numeric
                            } else {
                                band.from < numeric
                            }
                        })
                        .or_else(|| mapping.first())
                        .map_or(EffectiveReasoningEffort::Unsupported, |band| {
                            EffectiveReasoningEffort::Native(band.level)
                        })
                }
            },
        }
    }

    /// Returns whether this capability exposes an exact native level.
    #[must_use]
    pub fn contains(&self, level: crate::NativeReasoningEffort) -> bool {
        match &self.control {
            ReasoningEffortControl::Unsupported => false,
            ReasoningEffortControl::Fixed { level: fixed } => *fixed == level,
            ReasoningEffortControl::Mapped { mapping } => {
                mapping.iter().any(|band| band.level == level)
            }
        }
    }
}

/// Frozen prompt-time native reasoning selection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", content = "level", rename_all = "snake_case")]
pub enum EffectiveReasoningEffort {
    /// Omit the selector; an optional level records a documented provider
    /// default.
    ProviderDefault(Option<crate::NativeReasoningEffort>),
    /// Send this exact native selector.
    Native(crate::NativeReasoningEffort),
    /// The route has this fixed behavior but sends no selector.
    Fixed(crate::NativeReasoningEffort),
    /// The route has neither a usable selector nor an exact fixed-level claim.
    Unsupported,
}

impl Default for EffectiveReasoningEffort {
    fn default() -> Self {
        Self::ProviderDefault(None)
    }
}

impl EffectiveReasoningEffort {
    /// Returns the exact native level when one is known.
    #[must_use]
    pub const fn native(self) -> Option<crate::NativeReasoningEffort> {
        match self {
            Self::ProviderDefault(level) => level,
            Self::Native(level) | Self::Fixed(level) => Some(level),
            Self::Unsupported => None,
        }
    }

    /// Returns the selector that a provider should send.
    #[must_use]
    pub const fn selector(self) -> Option<crate::NativeReasoningEffort> {
        match self {
            Self::Native(level) => Some(level),
            Self::ProviderDefault(_) | Self::Fixed(_) | Self::Unsupported => None,
        }
    }
}

impl fmt::Display for EffectiveReasoningEffort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderDefault(None) => formatter.write_str("provider_default"),
            Self::ProviderDefault(Some(level)) => write!(formatter, "provider_default→{level}"),
            Self::Native(level) => level.fmt(formatter),
            Self::Fixed(level) => write!(formatter, "fixed:{level}"),
            Self::Unsupported => formatter.write_str("unsupported"),
        }
    }
}

/// Portable request plus its frozen prompt-time native result.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub struct ReasoningSelection {
    /// Unclamped portable request.
    pub requested: ReasoningIntent,
    /// Frozen model-specific result.
    pub effective: EffectiveReasoningEffort,
}

impl ReasoningSelection {
    /// Creates a direct native fixture selection with matching portable intent.
    #[must_use]
    pub const fn native(level: crate::NativeReasoningEffort) -> Self {
        let millionths = match level {
            crate::NativeReasoningEffort::None => 0,
            crate::NativeReasoningEffort::Minimal => 100_000,
            crate::NativeReasoningEffort::Low => 250_000,
            crate::NativeReasoningEffort::Medium => 500_000,
            crate::NativeReasoningEffort::High => 750_000,
            crate::NativeReasoningEffort::XHigh => 900_000,
            crate::NativeReasoningEffort::Max => 1_000_000,
        };
        Self {
            requested: ReasoningIntent::Intensity(ReasoningIntensity::from_millionths(millionths)),
            effective: EffectiveReasoningEffort::Native(level),
        }
    }
}

impl fmt::Display for ReasoningSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.effective {
            EffectiveReasoningEffort::ProviderDefault(None) => self.requested.fmt(formatter),
            EffectiveReasoningEffort::ProviderDefault(Some(level))
            | EffectiveReasoningEffort::Native(level)
            | EffectiveReasoningEffort::Fixed(level) => {
                write!(formatter, "{}→{level}", self.requested)
            }
            EffectiveReasoningEffort::Unsupported => {
                write!(formatter, "{}→unsupported", self.requested)
            }
        }
    }
}

/// Failure to parse an exact millionth reasoning intensity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseReasoningIntensityError {
    /// The decimal syntax is invalid.
    Invalid,
    /// More than six fractional digits were supplied.
    TooPrecise,
    /// The scaled value does not fit in `i32`.
    OutOfRange,
    /// A delta was zero.
    ZeroDelta,
}

impl fmt::Display for ParseReasoningIntensityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "invalid reasoning intensity decimal",
            Self::TooPrecise => "reasoning intensity supports at most six fractional digits",
            Self::OutOfRange => "reasoning intensity is outside the signed millionth range",
            Self::ZeroDelta => "reasoning intensity delta must be nonzero",
        })
    }
}

impl std::error::Error for ParseReasoningIntensityError {}

fn parse_millionths(input: &str) -> Result<i32, ParseReasoningIntensityError> {
    let input = input.trim();
    let (negative, unsigned) = input
        .strip_prefix('-')
        .map_or((false, input), |value| (true, value));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ParseReasoningIntensityError::Invalid);
    }
    if fraction.len() > 6 {
        return Err(ParseReasoningIntensityError::TooPrecise);
    }
    let whole: i64 = whole
        .parse()
        .map_err(|_| ParseReasoningIntensityError::OutOfRange)?;
    let fraction: i64 = if fraction.is_empty() {
        0
    } else {
        let digits: i64 = fraction
            .parse()
            .map_err(|_| ParseReasoningIntensityError::Invalid)?;
        digits * 10_i64.pow(6_u32.saturating_sub(fraction.len() as u32))
    };
    let scaled = whole
        .checked_mul(i64::from(REASONING_INTENSITY_ONE))
        .and_then(|value| value.checked_add(fraction))
        .ok_or(ParseReasoningIntensityError::OutOfRange)?;
    let scaled = if negative { -scaled } else { scaled };
    i32::try_from(scaled).map_err(|_| ParseReasoningIntensityError::OutOfRange)
}

fn format_millionths(value: i32, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let negative = value < 0;
    let magnitude = i64::from(value).unsigned_abs();
    let whole = magnitude / REASONING_INTENSITY_ONE as u64;
    let fraction = magnitude % REASONING_INTENSITY_ONE as u64;
    if negative {
        formatter.write_str("-")?;
    }
    if fraction == 0 {
        return write!(formatter, "{whole}.0");
    }
    let fraction = format!("{fraction:06}");
    write!(formatter, "{whole}.{}", fraction.trim_end_matches('0'))
}
