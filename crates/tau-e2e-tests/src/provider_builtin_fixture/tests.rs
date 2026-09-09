use super::*;

/// Keeps each script's complete YAML bytes, role/model pairing and executable
/// quoting stable when configuration rendering moves out of fixture startup.
#[test]
fn harness_configuration_preserves_exact_script_pairings() {
    let provider = Path::new("/fixture/provider \"quoted\"");
    let dummy = Path::new("/fixture/dummy");
    for (script, dummy, role, model, effort, tools, extension) in [
        (
            Script::Retry,
            None,
            "provider-builtin-retry",
            "local/retry-model",
            "",
            "[]",
            "",
        ),
        (
            Script::Qwen,
            Some(dummy),
            "provider-builtin-qwen",
            "local/Qwen/Qwen3.8-27B",
            "          effort: 1.0\n",
            "[restart_test_dummy]",
            "  e2e-test-dummy:\n    command: [\"/fixture/dummy\"]\n    role: tool\n    require: true\n    config:\n      restart_mode: success\n",
        ),
        (
            Script::Compaction,
            None,
            "provider-builtin-compaction",
            "local/llama-compaction-model",
            "",
            "[]",
            "",
        ),
        (
            Script::CompactionContinuation,
            None,
            "provider-builtin-compaction",
            "local/llama-compaction-model",
            "",
            "[]",
            "",
        ),
    ] {
        let expected = format!(
            concat!(
                "agents:\n",
                "  default_role: {role}\n",
                "  idTemplate: main\n",
                "  role_groups:\n",
                "    e2e:\n",
                "      roles:\n",
                "        {role}:\n",
                "          model: {model}\n",
                "{effort}",
                "          tools: {tools}\n",
                "extensions:\n",
                "  provider-builtin:\n",
                "    command: [\"/fixture/provider \\\"quoted\\\"\"]\n",
                "    role: provider\n",
                "    require: true\n",
                "{extension}",
                "  core-shell:\n    enable: false\n",
                "  test-dummy:\n    enable: false\n",
                "  std-rhai:\n    enable: false\n",
                "  std-rostra:\n    enable: false\n",
                "  std-notifications:\n    enable: false\n",
                "  std-slack:\n    enable: false\n",
                "  std-telegram:\n    enable: false\n",
                "  std-zulip:\n    enable: false\n",
                "  std-xmpp:\n    enable: false\n",
                "  std-utils:\n    enable: false\n",
                "  std-websearch:\n    enable: false\n",
                "  std-pim:\n    enable: false\n",
                "  std-email:\n    enable: false\n",
            ),
            role = role,
            model = model,
            effort = effort,
            tools = tools,
            extension = extension,
        );
        assert_eq!(
            ProviderBuiltinFixture::render_harness_config(script, provider, dummy)
                .expect("render fixture configuration"),
            expected,
            "{script:?}",
        );
    }
}
