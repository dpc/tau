//! Compact presentation for estimated equivalent API cost.

use tau_proto::EstimatedApiCost;

const PICODOLLARS_PER_CENT: u64 = 10_000_000_000;
const PICODOLLARS_PER_TENTH_DOLLAR: u64 = 100_000_000_000;
const PICODOLLARS_PER_DOLLAR: u64 = 1_000_000_000_000;
const PICODOLLARS_PER_THOUSAND_DOLLARS: u64 = 1_000 * PICODOLLARS_PER_DOLLAR;
const PICODOLLARS_PER_TENTH_MILLION_DOLLARS: u64 = 100_000 * PICODOLLARS_PER_DOLLAR;
const PICODOLLARS_PER_MILLION_DOLLARS: u64 = 1_000_000 * PICODOLLARS_PER_DOLLAR;

/// Format an estimated cost as `$` plus at most three characters.
///
/// Values round half up at each representation boundary: cents below one
/// dollar, tenths below ten dollars, whole dollars below one thousand, whole
/// thousands below one hundred thousand, tenths of a million below one million,
/// then whole millions. A rounded boundary promotes to the next representation.
#[must_use]
pub(crate) fn format_compact(cost: EstimatedApiCost) -> String {
    let picodollars = cost.as_picodollars();
    let cents = rounded_units(picodollars, PICODOLLARS_PER_CENT);
    if cents < 100 {
        return format!("$.{cents:02}");
    }
    let tenths = rounded_units(picodollars, PICODOLLARS_PER_TENTH_DOLLAR);
    if tenths < 100 {
        return format!("${}.{}", tenths / 10, tenths % 10);
    }
    let dollars = rounded_units(picodollars, PICODOLLARS_PER_DOLLAR);
    if dollars < 1_000 {
        return format!("${dollars}");
    }
    let thousands = rounded_units(picodollars, PICODOLLARS_PER_THOUSAND_DOLLARS);
    if thousands < 100 {
        return format!("${thousands}k");
    }
    let tenths_of_millions = rounded_units(picodollars, PICODOLLARS_PER_TENTH_MILLION_DOLLARS);
    if tenths_of_millions < 10 {
        return format!("$.{tenths_of_millions}m");
    }
    let millions = rounded_units(picodollars, PICODOLLARS_PER_MILLION_DOLLARS);
    format!("${millions}m")
}

fn rounded_units(value: u64, unit: u64) -> u64 {
    u64::try_from((u128::from(value) + u128::from(unit / 2)) / u128::from(unit)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
