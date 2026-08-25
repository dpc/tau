use super::*;

/// Defaults must leave fixed request/output reserves and derive a conservative
/// proactive threshold from the configured prefix budget.
#[test]
fn defaults_fit_context_and_publish_conservative_threshold() {
    let config = Config::default_for(128_000).expect("ordinary model context");
    assert_eq!(config.max_output_tokens(), 4096);
    assert_eq!(config.max_input_bytes(), 122_880);
    assert_eq!(config.proactive_threshold(), 30_720);
    assert!(Config::default_for(1315).is_none());
    assert_eq!(
        Config::default_for(1316)
            .expect("exact viable boundary")
            .proactive_threshold(),
        64
    );
}

/// The shared instruction must identify harness authority and forbid tools
/// without replacing the ordinary system prompt.
#[test]
fn request_is_the_cache_aligned_trailing_user_instruction() {
    assert!(REQUEST.starts_with("<tau_internal>\n"));
    assert!(REQUEST.ends_with("\n&lt;/tau_internal&gt;"));
    assert!(REQUEST.contains("Do not make or request any tool calls."));
    assert!(REQUEST.contains("Return only the summary."));

    let too_small = Config::new(
        NonZeroU64::new(2048).expect("positive"),
        2048,
        NonZeroU64::new(255).expect("positive"),
        NonZeroU32::new(1).expect("positive"),
        NonZeroU64::new(1).expect("positive"),
    );
    assert!(too_small.is_none());
}
