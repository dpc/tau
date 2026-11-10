use super::*;

/// Completion after prose contractions must transmit only the cursor token and
/// replace only that token, preserving the earlier README and UTF-8
/// surroundings.
#[test]
fn prose_paths_preserve_draft_and_cursor_on_reply() {
    let literal = "./README.md looks like shit. It supposed to be a human-facing informative, high level intro to\nproject, and instead it's full of commits and super low level dev details. You can look how ";
    for (before, word, prefix, after) in [
        (literal, "./", "./", ""),
        ("doesn't trigger ", "./", "./", ""),
        ("l'été\nopen ", "'./hé llo'", "'./hé", " résumé"),
        ("it's ", "./héllo", "./hé", " 世界"),
    ] {
        let buffer = format!("{before}{word}{after}");
        let cursor = before.len() + prefix.len();
        let parsed = token(&buffer, cursor).expect("cursor token");
        assert_eq!(parsed.before, before);
        assert_eq!(parsed.after, after);
        assert_eq!(parsed.decoded, prefix.trim_start_matches('\''));
        let (cache, _receiver) = cache();
        let context = Context {
            session_id: "session".parse().expect("session"),
            agent_id: None,
            request_id: "prose".to_owned(),
        };
        let target = Target {
            instance: "remote".parse().expect("instance"),
            nonce: "process".to_owned(),
        };
        {
            let mut state = cache.state.lock().expect(MUTEX_POISONED);
            state.draft = Some(Draft {
                generation: 0,
                buffer,
                cursor,
                agent: None,
                targets: vec![target.clone()],
                fuzzy: false,
            });
            state.pending.insert(
                context.request_id.clone(),
                Pending {
                    context: context.clone(),
                    target: target.clone(),
                    deadline: Instant::now() + Duration::from_secs(1),
                },
            );
        }
        assert!(cache.accept(
            context,
            target,
            vec![PathCandidate {
                value: "./hé llo".to_owned(),
                directory: false,
            }],
        ));
        let state = cache.state.lock().expect(MUTEX_POISONED);
        let result = &state.results[0];
        let insertion = "./hé\\ llo";
        assert_eq!(result.replacement, format!("{before}{insertion}{after}"));
        assert_eq!(result.cursor, before.len() + insertion.len());
        assert!(result.replacement.is_char_boundary(result.cursor));
    }
}
