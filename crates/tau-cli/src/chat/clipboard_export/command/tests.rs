use super::*;

/// Explicit file intent preserves quoted Unicode names and never accepts path
/// authority; an omitted name is resolved only after verified byte inference.
#[test]
fn explicit_file_grammar_and_safe_names() {
    let key = "a".repeat(24);
    let default = Command::parse(&format!("{key} --file")).expect("default file command");
    assert!(matches!(default.mode, Mode::File(None)));
    let plain_uri = Command::parse(&format!("tau-artifact:{key} --file"))
        .expect("copyable plain UI artifact locator");
    assert!(matches!(plain_uri.mode, Mode::File(None)));
    for name in [
        "\"two words.pdf\"",
        "'résumé final.pdf'",
        "\"it's fine.txt\"",
    ] {
        let parsed = Command::parse(&format!("<tau-artifact:{key}> --file --name {name}"))
            .expect("quoted name");
        let Mode::File(Some(parsed_name)) = parsed.mode else {
            panic!("explicit file name")
        };
        assert_eq!(parsed_name.as_str(), &name[1..name.len() - 1]);
    }
    for args in [
        "--name a",
        "--file --mime text/plain",
        "--mime text/plain --file",
        "--file --name",
        "--file --name ''",
        "--file --name .",
        "--file --name ..",
        "--file --name /tmp/a",
        "--file --name a\\b",
        "--file --name \"a\nb\"",
        "--file --name a b",
        "--file --name 'unfinished",
        "--file --name 'a' trailing",
        "--file --name \"a\" \"b\"",
        "--file --file",
        "--raw --file",
        "--file --raw",
        "--raw --name a",
        "--raw --raw",
        "--raw --mime",
        "--raw --mime text/plain extra",
        "--mime text/plain --raw",
    ] {
        assert!(Command::parse(&format!("{key} {args}")).is_err(), "{args}");
    }
    for name in ["x".repeat(255), "é".repeat(127)] {
        assert!(Command::parse(&format!("{key} --file --name {name}")).is_ok());
    }
    for name in ["x".repeat(256), "é".repeat(128)] {
        assert!(Command::parse(&format!("{key} --file --name {name}")).is_err());
    }
    assert!(matches!(
        Command::parse(&key).expect("automatic default").mode,
        Mode::Auto
    ));
    let raw = Command::parse(&format!("{key} --mime application/vnd.tau.file-export-v1"))
        .expect("raw MIME override");
    assert!(
        matches!(raw.mode, Mode::Raw(Some(mime)) if mime == "application/vnd.tau.file-export-v1")
    );
    assert!(matches!(
        Command::parse(&format!("{key} --raw")).expect("raw").mode,
        Mode::Raw(None)
    ));
    assert!(matches!(
        Command::parse(&format!("{key} --raw --mime image/svg+xml")).expect("raw MIME").mode,
        Mode::Raw(Some(mime)) if mime == "image/svg+xml"
    ));
}
