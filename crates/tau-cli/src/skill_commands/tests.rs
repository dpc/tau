use super::*;

/// Ensures `:skill` completion exposes only user-invocable skills and keeps
/// argument hints in the user-facing menu text.
#[test]
fn completes_only_user_invocable_skills() {
    let state = SkillCommandState::new();
    let skill = |name: &str, description: &str, user_invocable, argument_hint: Option<&str>| {
        tau_proto::DiscoveryEffectiveSkill {
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
        session_id: "session-1".into(),
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
