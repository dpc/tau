use super::*;

/// Ensures `:skill` completion exposes only user-invocable skills and keeps
/// argument hints in the user-facing menu text.
#[test]
fn completes_only_user_invocable_skills() {
    let state = SkillCommandState::new();
    let skill = |name: &str, description: &str, user_invocable, argument_hint: Option<&str>| {
        tau_proto::DiscoveryEffectiveSkill {
            visibility: Default::default(),
            name: name.into(),
            description: description.to_owned(),
            source: tau_proto::DiscoveryEffectiveSkillSource::File {
                path: format!("/tmp/{name}/SKILL.md").into(),
            },
            add_to_prompt: false,
            user_invocable,
            disable_model_invocation: name == "manual",
            argument_hint: argument_hint.map(str::to_owned),
        }
    };
    state.apply_session_snapshot(&tau_proto::HarnessSessionSkillsAvailable {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        skills: vec![
            skill("visible", "Visible skill", true, Some("[topic]")),
            skill("hidden", "Hidden skill", false, None),
            skill("manual", "Manual-only skill", true, Some("<task>")),
        ],
    });

    let completions = (state.arg_completer())(&[""]);
    assert_eq!(completions.len(), 2);
    assert_eq!(completions[0].value, "manual");
    assert!(completions[0].description.contains("<task>"));
    assert_eq!(completions[1].value, "visible");
    assert!(completions[1].description.contains("[topic]"));
}

/// The initialization summary must aggregate every available skill omitted from
/// the agent prompt, including skills that are not user-invocable.
#[test]
fn counts_available_skills_not_advertised_to_the_agent() {
    let state = SkillCommandState::new();
    let skill = |name: &str, user_invocable| tau_proto::DiscoveryEffectiveSkill {
        visibility: Default::default(),
        name: name.into(),
        description: format!("{name} description"),
        source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
        add_to_prompt: true,
        user_invocable,
        disable_model_invocation: false,
        argument_hint: None,
    };
    let advertised = skill("advertised", true);
    state.apply_session_snapshot(&tau_proto::HarnessSessionSkillsAvailable {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        skills: vec![
            advertised.clone(),
            skill("manual", true),
            skill("hidden", false),
        ],
    });

    assert_eq!(state.unadvertised_count(&[advertised]), 2);
}

/// Existing-agent completion uses only its frozen eligible projection, while
/// new-agent completion filters the current baseline by exact role/group.
#[test]
fn context_completion_switches_frozen_agents_and_prospective_roles() {
    let state = SkillCommandState::new();
    let skill = |name: &str, role: &str| tau_proto::DiscoveryEffectiveSkill {
        visibility: tau_proto::ContextVisibility {
            only_role_groups: Some(vec![role.to_owned()]),
            ..Default::default()
        },
        name: name.into(),
        description: name.to_owned(),
        source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
        add_to_prompt: false,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
    };
    let first = skill("first", "engineering");
    let second = skill("second", "custom");
    let session_id: tau_proto::SessionId = "session-1".parse().expect("session");
    let baseline = tau_proto::HarnessSessionSkillsAvailable {
        session_id: session_id.clone(),
        skills: vec![first.clone(), second.clone()],
    };
    state.apply_session_snapshot(&baseline);
    let names = || {
        state
            .complete_args(&[""])
            .into_iter()
            .map(|item| item.value)
            .collect::<Vec<_>>()
    };
    state.set_prospective_role("senior".to_owned(), "engineering".to_owned());
    assert_eq!(names(), ["first"]);
    state.set_prospective_role("custom".to_owned(), "custom".to_owned());
    assert_eq!(names(), ["second"]);
    for (agent, skills) in [
        ("first-agent", vec![first]),
        ("second-agent", vec![second]),
        ("old-peer-agent", Vec::new()),
    ] {
        state.apply_agent_snapshot(&tau_proto::HarnessAgentContextInitialized {
            session_id: session_id.clone(),
            agent_id: agent.parse().expect("agent"),
            agent_initialization_id: "init".parse().expect("initialization"),
            listed_skills: Vec::new(),
            effective_skills: skills,
            agents_files: Vec::new(),
        });
    }
    state.select_agent(Some("first-agent".parse().expect("agent")));
    assert_eq!(names(), ["first"]);
    state.apply_session_snapshot(&tau_proto::HarnessSessionSkillsAvailable {
        session_id,
        skills: Vec::new(),
    });
    assert_eq!(
        names(),
        ["first"],
        "session refresh must not change frozen completion"
    );
    state.select_agent(Some("second-agent".parse().expect("agent")));
    assert_eq!(names(), ["second"]);
    for agent in ["old-peer-agent", "uninitialized-agent"] {
        state.select_agent(Some(agent.parse().expect("agent")));
        assert!(
            names().is_empty(),
            "missing projection must not fall back to session inventory"
        );
    }
}
