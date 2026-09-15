use super::*;

/// Markdown output must expose each requested field and distinguish built-in
/// skills from filesystem-backed skills.
#[test]
fn markdown_lists_name_path_and_description() {
    let skills = vec![
        SkillPreview {
            name: tau_proto::SkillName::from("alpha"),
            path: Some(PathBuf::from("/tmp/alpha/SKILL.md")),
            description: "Alpha description.".to_owned(),
        },
        SkillPreview {
            name: tau_proto::SkillName::from("builtin"),
            path: None,
            description: "Built-in description.".to_owned(),
        },
    ];
    let mut output = Vec::new();

    write_skills(&mut output, SkillOutputFormat::Markdown, &skills)
        .expect("render Markdown skills");

    assert_eq!(
        String::from_utf8(output).expect("UTF-8 Markdown"),
        "# Available skills\n\n\
         ## `alpha`\n\
         **Path:** `/tmp/alpha/SKILL.md`\n\n\
         Alpha description.\n\n\
         ## `builtin`\n\
         **Path:** built-in\n\n\
         Built-in description.\n\n"
    );
}

/// JSON output must retain a null path for built-ins so consumers can
/// distinguish them without parsing a presentation label.
#[test]
fn json_is_machine_readable_and_preserves_builtin_origin() {
    let skills = vec![SkillPreview {
        name: tau_proto::SkillName::from("builtin"),
        path: None,
        description: "Built-in description.".to_owned(),
    }];
    let mut output = Vec::new();

    write_skills(&mut output, SkillOutputFormat::Json, &skills).expect("render JSON skills");

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output).expect("parse JSON skills"),
        serde_json::json!([{
            "name": "builtin",
            "path": null,
            "description": "Built-in description."
        }])
    );
}

/// Markdown code spans must remain structurally valid for unusual but legal
/// filesystem names containing backticks.
#[test]
fn markdown_uses_a_longer_fence_for_backticks_in_paths() {
    let skills = vec![SkillPreview {
        name: tau_proto::SkillName::from("unusual"),
        path: Some(PathBuf::from("/tmp/a`b/SKILL.md")),
        description: "Unusual source path.".to_owned(),
    }];
    let mut output = Vec::new();

    write_skills(&mut output, SkillOutputFormat::Markdown, &skills)
        .expect("render Markdown skills");

    assert!(
        String::from_utf8(output)
            .expect("UTF-8 Markdown")
            .contains("**Path:** `` /tmp/a`b/SKILL.md ``")
    );
}
