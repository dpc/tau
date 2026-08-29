use super::*;

/// Builds a registration whose full ordering identity is explicit in each
/// successor-selection oracle.
fn registration(
    priority: i64,
    component_name: &str,
    connection_id: &str,
) -> InterceptorRegistration {
    InterceptorRegistration {
        priority: InterceptionPriority::new(priority),
        component_name: crate::test_extension_name(component_name),
        connection_id: crate::test_connection_id(connection_id),
    }
}

/// Builds a minimal event whose name can match a `harness` prefix selector.
fn notice() -> Event {
    Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.notice".to_owned(),
        message: "test".to_owned(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    })
}

/// Ensures empty, first, exact-cursor, missing-cursor, same-priority, and
/// end lookups preserve the complete registration order.
#[test]
fn successor_lookup_preserves_full_registration_order() {
    let registry = InterceptorRegistry::default();
    let empty = BTreeSet::new();
    assert_eq!(registry.next_in_set(Some(&empty), None), None);

    let alpha = registration(0, "alpha", "alpha-connection");
    let beta = registration(0, "beta", "beta-connection");
    let gamma = registration(0, "beta", "gamma-connection");
    let registrations = BTreeSet::from([gamma.clone(), beta.clone(), alpha.clone()]);

    assert_eq!(
        registry.next_in_set(Some(&registrations), None),
        Some(alpha.clone()),
        "the first lookup must return the first full registration"
    );
    assert_eq!(
        registry.next_in_set(Some(&registrations), Some(&alpha)),
        Some(beta.clone()),
        "an exact cursor must advance to the next same-priority registration"
    );
    assert_eq!(
        registry.next_in_set(
            Some(&registrations),
            Some(&registration(0, "aardvark", "missing-connection"))
        ),
        Some(alpha),
        "a missing cursor must select its ordered successor"
    );
    assert_eq!(
        registry.next_in_set(Some(&registrations), Some(&beta)),
        Some(gamma.clone()),
        "connection identity must break same-component priority ties"
    );
    assert_eq!(
        registry.next_in_set(Some(&registrations), Some(&gamma)),
        None,
        "the final registration must not wrap to the set prefix"
    );
}

/// Ensures prefix registration chains use the same successor rule and that
/// removing the current registration leaves the next live registration.
#[test]
fn prefix_successor_skips_removed_registration() {
    let mut registry = InterceptorRegistry::default();
    let alpha_connection = crate::test_connection_id("alpha-connection");
    let beta_connection = crate::test_connection_id("beta-connection");
    registry.replace_for_connection(
        &alpha_connection,
        crate::test_extension_name("alpha"),
        vec![EventSelector::Prefix("harness".to_owned())],
        InterceptionPriority::new(0),
    );
    registry.replace_for_connection(
        &beta_connection,
        crate::test_extension_name("beta"),
        vec![EventSelector::Prefix("harness".to_owned())],
        InterceptionPriority::new(0),
    );

    let first = registry
        .next_for(&notice(), None)
        .expect("prefix selector must find its first registration");
    assert_eq!(first.set, InterceptorSet::Prefix);
    assert_eq!(first.registration.connection_id, alpha_connection);

    registry.remove_connection(&alpha_connection);
    let successor = registry
        .next_for(
            &notice(),
            Some(&InterceptorCursor {
                set: first.set,
                registration: first.registration,
            }),
        )
        .expect("removed cursor registration must not hide its successor");
    assert_eq!(successor.set, InterceptorSet::Prefix);
    assert_eq!(successor.registration.connection_id, beta_connection);
}

/// Demonstrates that a long same-set continuation performs bounded
/// successor searches instead of repeatedly comparing the consumed prefix.
#[test]
fn successor_lookup_avoids_quadratic_prefix_comparisons() {
    const REGISTRATION_COUNT: usize = 128;
    let registry = InterceptorRegistry::default();
    let registrations = (0..REGISTRATION_COUNT)
        .map(|index| registration(0, &format!("interceptor-{index:03}"), "connection"))
        .collect::<BTreeSet<_>>();

    reset_registration_order_comparisons();
    let mut cursor = None;
    for _ in 0..REGISTRATION_COUNT {
        let registration = registry
            .next_in_set(Some(&registrations), cursor.as_ref())
            .expect("each registration must have one successor lookup");
        cursor = Some(registration);
    }
    assert_eq!(
        registry.next_in_set(Some(&registrations), cursor.as_ref()),
        None,
        "the chain must end after every registration"
    );

    let comparisons = registration_order_comparisons();
    assert!(
        comparisons < REGISTRATION_COUNT * 16,
        "{comparisons} ordering comparisons revisited too much of the consumed prefix"
    );
}

/// The one-shot prompt handoff must preserve the owned string allocation
/// instead of deep-cloning the complete materialized request.
#[test]
fn unique_prompt_handoff_moves_constituent_allocations() {
    let prompt = tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-move-owned".parse().expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("agent-move-owned").expect("agent id"),
        session_id: tau_proto::SessionId::parse("session-move-owned").expect("session id"),
        system_prompt: "uniquely owned system prompt".to_owned(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    };
    let system_prompt_address = prompt.system_prompt.as_ptr();

    let continuation = super::PromptDispatchContinuation {
        authority: super::PromptDispatchAuthority {
            started: tau_proto::AgentPromptStarted::from(&prompt),
            provider_connection_id: tau_proto::ConnectionId::parse("provider-move-owned")
                .expect("provider connection"),
            runtime_incarnation: 7,
            materialization_timing: None,
        },
        prompt: Arc::new(prompt),
    };

    let (moved, authority) = continuation.into_delivery();

    assert_eq!(moved.system_prompt.as_ptr(), system_prompt_address);
    assert_eq!(moved.system_prompt, "uniquely owned system prompt");
    assert_eq!(authority.runtime_incarnation, 7);
}
