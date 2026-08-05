use super::*;
use crate::ProviderCacheUsage;

fn usage(input: u64, cached: u64, output: u64) -> ProviderTokenUsage {
    ProviderTokenUsage {
        prompt_sent_tokens: input,
        prompt_cached_tokens: cached,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: output,
        ..ProviderTokenUsage::default()
    }
}

/// Cached, uncached, and output token classes use their independent rates.
#[test]
fn fixed_point_cost_covers_all_usage_classes() {
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(2).expect("test price"),
        cached_input: EstimatedUsdPerMillion::from_micro_usd(500_000),
        cache_write_input: None,
        output: EstimatedUsdPerMillion::checked_from_usd(10).expect("test price"),
        storage_per_million_token_hour: None,
    };
    let mut cost = EstimatedApiCost::default();

    cost.add_usage(&usage(1_000_000, 250_000, 100_000), rates);

    assert_eq!(cost.as_picodollars(), 2_625_000_000_000);
}

/// Missing cache telemetry conservatively prices every input token as
/// uncached with the universal fallback, including a local/free route.
#[test]
fn missing_cache_detail_uses_uncached_fallback_price() {
    let mut cost = EstimatedApiCost::default();

    cost.add_usage(&usage(1_000_000, 0, 0), ESTIMATED_API_COST_FALLBACK);

    assert_eq!(cost.as_picodollars(), 5_000_000_000_000);
}

/// Sequential records retain the rate selected for each serving model
/// rather than repricing the accumulated token total after a model
/// change.
#[test]
fn mixed_provider_model_records_accumulate_incrementally() {
    let cheap = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("test price"),
        cached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("test price"),
        cache_write_input: None,
        output: EstimatedUsdPerMillion::checked_from_usd(1).expect("test price"),
        storage_per_million_token_hour: None,
    };
    let expensive = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(10).expect("test price"),
        cached_input: EstimatedUsdPerMillion::checked_from_usd(10).expect("test price"),
        cache_write_input: None,
        output: EstimatedUsdPerMillion::checked_from_usd(10).expect("test price"),
        storage_per_million_token_hour: None,
    };
    let mut cost = EstimatedApiCost::default();

    cost.add_usage(&usage(1_000_000, 0, 0), cheap);
    cost.add_usage(&usage(1_000_000, 0, 0), expensive);

    assert_eq!(cost.as_picodollars(), 11_000_000_000_000);
}

/// Extremely large usage saturates instead of wrapping a lifetime estimate.
#[test]
fn cost_accumulation_saturates() {
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::from_micro_usd(u64::MAX),
        cached_input: EstimatedUsdPerMillion::from_micro_usd(u64::MAX),
        cache_write_input: None,
        output: EstimatedUsdPerMillion::from_micro_usd(u64::MAX),
        storage_per_million_token_hour: None,
    };
    let mut cost = EstimatedApiCost::default();

    cost.add_usage(&usage(u64::MAX, 0, u64::MAX), rates);

    assert_eq!(cost.as_picodollars(), u64::MAX);
}

/// Ordinary input, cache reads, cache writes, output, and token-time storage
/// contribute as five independent cost classes.
#[test]
fn cost_prices_cache_writes_and_storage_independently() {
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(2).expect("ordinary rate"),
        cached_input: EstimatedUsdPerMillion::from_micro_usd(500_000),
        cache_write_input: Some("2.5".parse::<EstimatedUsdPerMillion>().expect("write rate")),
        output: EstimatedUsdPerMillion::checked_from_usd(10).expect("output rate"),
        storage_per_million_token_hour: Some(
            EstimatedUsdPerMillionTokenHours::checked_from_usd(4).expect("storage rate"),
        ),
    };
    let mut record = usage(1_000_000, 200_000, 100_000);
    record.cache = Some(Box::new(ProviderCacheUsage {
        read_tokens: Some(200_000),
        write_tokens: Some(300_000),
        storage_token_micros: Some(crate::CacheStorageTokenMicros::new(3_600_000_000_000_000)),
        ..ProviderCacheUsage::default()
    }));

    let cost = EstimatedApiCost::for_usage(&record, rates);

    assert_eq!(cost.as_picodollars(), 6_850_000_000_000);
}

/// Contradictory cache counters clamp in read-then-write order and cannot
/// create more priced input than the provider's authoritative input total.
#[test]
fn contradictory_cache_classes_clamp_to_total_input() {
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("ordinary rate"),
        cached_input: EstimatedUsdPerMillion::checked_from_usd(2).expect("read rate"),
        cache_write_input: Some(EstimatedUsdPerMillion::checked_from_usd(3).expect("write rate")),
        output: EstimatedUsdPerMillion::checked_from_usd(1).expect("output rate"),
        storage_per_million_token_hour: None,
    };
    let mut record = usage(1_000_000, 2_000_000, 0);
    record.cache = Some(Box::new(ProviderCacheUsage {
        read_tokens: Some(2_000_000),
        write_tokens: Some(2_000_000),
        ..ProviderCacheUsage::default()
    }));

    let cost = EstimatedApiCost::for_usage(&record, rates);

    assert_eq!(cost.as_picodollars(), 2_000_000_000_000);
}

/// A cache write without a specific premium retains the published behavior of
/// pricing that input at the ordinary input rate.
#[test]
fn missing_cache_write_rate_uses_ordinary_input_rate() {
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(2).expect("ordinary rate"),
        cached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("read rate"),
        cache_write_input: None,
        output: EstimatedUsdPerMillion::checked_from_usd(1).expect("output rate"),
        storage_per_million_token_hour: None,
    };
    let mut record = usage(1_000_000, 0, 0);
    record.cache = Some(Box::new(ProviderCacheUsage {
        write_tokens: Some(1_000_000),
        ..ProviderCacheUsage::default()
    }));

    assert_eq!(
        EstimatedApiCost::for_usage(&record, rates).as_picodollars(),
        2_000_000_000_000
    );
}

/// Profile prices accept exact strings or integer JSON numbers and reject
/// negative, malformed, fractional numeric, and over-precise values.
#[test]
fn estimated_price_validation_is_explicit() {
    let maximum = "18446744073709.551615"
        .parse::<EstimatedUsdPerMillion>()
        .expect("maximum fixed-point price");
    assert_eq!(maximum.as_micro_usd(), u64::MAX);
    assert_eq!(
        serde_json::to_string(&maximum).expect("serialize exact price"),
        r#""18446744073709.551615""#
    );
    assert_eq!(
        serde_json::from_str::<EstimatedUsdPerMillion>(
            &serde_json::to_string(&maximum).expect("serialize round trip")
        )
        .expect("deserialize round trip"),
        maximum
    );
    assert_eq!(
        serde_json::from_str::<EstimatedUsdPerMillion>(r#""0.075""#)
            .expect("decimal string")
            .as_micro_usd(),
        75_000
    );
    assert_eq!(
        serde_json::from_str::<EstimatedUsdPerMillion>("2")
            .expect("integer JSON number")
            .as_micro_usd(),
        2_000_000
    );
    for invalid in [
        r#""-1""#,
        "-1",
        "2.5",
        "1.00000001",
        "9007199254740992.000001",
        r#""1.0000001""#,
        r#""wat""#,
        "1e100",
    ] {
        assert!(
            serde_json::from_str::<EstimatedUsdPerMillion>(invalid).is_err(),
            "{invalid} must fail"
        );
    }

    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
        let mut encoded = Vec::new();
        ciborium::into_writer(&invalid, &mut encoded).expect("encode invalid CBOR float");
        assert!(
            ciborium::from_reader::<EstimatedUsdPerMillion, _>(encoded.as_slice()).is_err(),
            "{invalid:?} CBOR value must fail"
        );
    }
    assert!(
        EstimatedUsdPerMillion::checked_from_usd(u64::MAX).is_none(),
        "whole-dollar construction must reject overflow"
    );
}
