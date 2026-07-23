use super::*;

/// Ensures `:skill` completion exposes only user-invocable skills and keeps
/// argument hints in the user-facing menu text.
#[test]
fn completes_only_user_invocable_skills() {
    let state = SkillCommandState::new();
    state.apply_skill_available(&ExtSkillAvailable {
        name: "visible".into(),
        description: "Visible skill".to_owned(),
        file_path: "/tmp/visible/SKILL.md".into(),
        add_to_prompt: false,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: Some("[topic]".to_owned()),
    });
    state.apply_skill_available(&ExtSkillAvailable {
        name: "hidden".into(),
        description: "Hidden skill".to_owned(),
        file_path: "/tmp/hidden/SKILL.md".into(),
        add_to_prompt: false,
        user_invocable: false,
        disable_model_invocation: false,
        argument_hint: None,
    });
    state.apply_skill_available(&ExtSkillAvailable {
        name: "manual".into(),
        description: "Manual-only skill".to_owned(),
        file_path: "/tmp/manual/SKILL.md".into(),
        add_to_prompt: false,
        user_invocable: false,
        disable_model_invocation: true,
        argument_hint: Some("<task>".to_owned()),
    });

    let completions = (state.arg_completer())(&[""]);
    assert_eq!(completions.len(), 2);
    assert_eq!(completions[0].value, "manual");
    assert!(completions[0].description.contains("<task>"));
    assert_eq!(completions[1].value, "visible");
    assert!(completions[1].description.contains("[topic]"));
}
