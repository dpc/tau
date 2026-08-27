use super::*;

/// Generic fallback derives only a same-domain token output cap and publishes
/// no fabricated prefix byte cap.
#[test]
fn defaults_publish_no_cross_unit_limits() {
    let config = Config::default_for(128_000).expect("ordinary model context");
    assert_eq!(config.max_output_tokens(), 4096);
    assert_eq!(config.max_input_bytes(), None);
    assert!(Config::default_for(0).is_none());
}

/// The shared instruction must identify harness authority and forbid tools
/// without replacing the ordinary system prompt.
#[test]
fn request_is_the_cache_aligned_trailing_user_instruction() {
    assert!(REQUEST.starts_with("<tau_internal>\n"));
    assert!(REQUEST.ends_with("\n&lt;/tau_internal&gt;"));
    assert!(REQUEST.contains("Do not make or request any tool calls."));
    assert!(REQUEST.contains("Return only the summary."));

    let independent_byte_cap = Config::new(
        NonZeroU64::new(2048).expect("positive"),
        2048,
        NonZeroU64::new(255).expect("positive"),
        NonZeroU32::new(1).expect("positive"),
        NonZeroU64::new(1).expect("positive"),
    );
    assert_eq!(
        independent_byte_cap
            .expect("byte work cap is independent of token capacity")
            .max_input_bytes(),
        Some(tau_proto::ByteCount::new(255))
    );
}
