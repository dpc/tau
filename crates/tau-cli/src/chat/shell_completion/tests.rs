use std::fmt::Write as _;
use std::path::Path;
use std::sync::Condvar;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tau_cli_term_raw::{Event as RawTermEvent, RawEvent};

use super::super::{DraftSlot, handle_prompt_buffer_changed};
use super::*;

/// Chat's production draft handler must preserve ordinary completion cycling
/// and Escape restoration; stale endpoint refreshes must not reopen that menu.
#[test]
fn chat_buffer_changes_preserve_completion_preview_and_dismissal() {
    let (cache, _receiver) = cache();
    let routing = InputRoutingState::new(
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Default::default())),
    );
    let drafts = (Mutex::new(DraftSlot::default()), Condvar::new());
    let (mut term, handle, input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    let source_handle = handle.clone();
    let source_cache = cache.clone();
    let context = Context {
        session_id: "session".parse().expect("session"),
        agent_id: None,
        request_id: "late-path".to_owned(),
    };
    let target = Target {
        instance: "remote".parse().expect("instance"),
        nonce: "process".to_owned(),
    };
    term.set_completion_source(Some(Box::new(move |buffer: &str, _cursor: usize| {
        source_cache
            .state
            .lock()
            .expect(MUTEX_POISONED)
            .menu_generation = Some(source_handle.completion_refresh_generation());
        if buffer != ":" {
            return Vec::new();
        }
        [":first", ":second"]
            .into_iter()
            .map(|value| Candidate {
                label: value.to_owned(),
                description: "ordinary command".to_owned(),
                replacement: value.to_owned(),
                cursor: value.len(),
                acceptance: None,
            })
            .collect()
    })));
    for (key, expected) in [
        (KeyCode::Char(':'), ":"),
        (KeyCode::Tab, ":first"),
        (KeyCode::Tab, ":second"),
        (KeyCode::Esc, ":"),
    ] {
        input
            .send(RawEvent::Key(KeyEvent::new(key, KeyModifiers::NONE)))
            .expect("key");
        assert!(matches!(
            term.get_next_event().expect("change"),
            RawTermEvent::BufferChanged
        ));
        handle_prompt_buffer_changed(
            &handle,
            &routing,
            &drafts,
            "session".parse().expect("session"),
        );
        if key != KeyCode::Char(':') {
            cache.observe(
                context.clone(),
                target.clone(),
                vec![PathCandidate {
                    value: "./late".to_owned(),
                    directory: false,
                }],
                &handle,
            );
            cache.refresh(&handle);
            assert!(cache.state.lock().expect(MUTEX_POISONED).results.is_empty());
        } else {
            let mut state = cache.state.lock().expect(MUTEX_POISONED);
            state.draft = Some(Draft {
                generation: handle.completion_refresh_generation(),
                buffer: "./".to_owned(),
                cursor: 2,
                agent: None,
                targets: vec![target.clone()],
                fuzzy: false,
            });
            state.pending.insert(
                context.request_id.clone(),
                Pending {
                    context: context.clone(),
                    target: target.clone(),
                    deadline: Instant::now() + Duration::from_secs(5),
                },
            );
        }
        input
            .send(RawEvent::FocusChanged { focused: true })
            .expect("wake");
        assert!(matches!(
            term.get_next_event().expect("wake"),
            RawTermEvent::FocusChanged { .. }
        ));
        assert_eq!(handle.get_buffer(), expected);
    }
    // Moving to unrelated text does not consult the remote path source. The
    // generation guard must still reject its pending response.
    handle.set_buffer("ordinary text".to_owned(), 13);
    cache.observe(
        context,
        target,
        vec![PathCandidate {
            value: "./late".to_owned(),
            directory: false,
        }],
        &handle,
    );
    cache.refresh(&handle);
    input
        .send(RawEvent::FocusChanged { focused: false })
        .expect("wake");
    assert!(matches!(
        term.get_next_event().expect("wake"),
        RawTermEvent::FocusChanged { .. }
    ));
    assert_eq!(handle.get_buffer(), "ordinary text");
    assert!(cache.state.lock().expect(MUTEX_POISONED).results.is_empty());
}

/// Create a cache without launching its debounce worker.
fn cache() -> (ShellCompletion, mpsc::Receiver<()>) {
    let (wake, receiver) = mpsc::sync_channel(1);
    (
        ShellCompletion {
            wake,
            state: Arc::new(Mutex::new(State::default())),
        },
        receiver,
    )
}

/// Endpoint selection is explicit for bangs, bounded for ordinary paths,
/// and cursor changes abandon old correlations while edit bursts coalesce.
#[test]
fn remote_completion_bounds_fanout_and_invalidates_cursor_revision() {
    let mut client = Client::new("session".parse().expect("session"));
    let discover = client.discover();
    for index in 0..40 {
        let reply = tau_shell_interaction::Reply {
            context: discover.context.clone(),
            target: Target {
                instance: format!("shell-{index:02}").parse().expect("instance"),
                nonce: tau_shell_interaction::nonce(),
            },
            response: tau_shell_interaction::Response::Available {
                execute: true,
                complete: true,
            },
        };
        let tau_proto::HarnessInputMessage::Emit(emit) =
            tau_shell_interaction::reply_message(&reply)
        else {
            unreachable!()
        };
        client.observe_parts(
            &emit.event,
            Some(&tau_proto::EventPublisher::Extension {
                name: reply.target.instance,
                instance_id: tau_proto::ExtensionInstanceId::new(index + 1),
                connection_id: format!("connection-{index}").parse().expect("connection"),
            }),
            false,
        );
    }
    let (cache, receiver) = cache();
    let routing = InputRoutingState::new(
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Default::default())),
    );
    let (_term, handle, _input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    let complete = cache.completer(Arc::new(Mutex::new(client)), routing, handle);
    assert!(complete("open ./a", 8, Some(false)).is_empty());
    assert_eq!(
        cache
            .state
            .lock()
            .expect(MUTEX_POISONED)
            .draft
            .as_ref()
            .expect("draft")
            .targets
            .len(),
        32
    );
    assert!(complete("!shell-39 cat ./a", 16, Some(false)).is_empty());
    let revision = {
        let state = cache.state.lock().expect(MUTEX_POISONED);
        let targets = &state.draft.as_ref().expect("draft").targets;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].instance.as_str(), "shell-39");
        state.revision
    };
    complete("!shell-39 cat ./a", 15, Some(false));
    assert_ne!(cache.state.lock().expect(MUTEX_POISONED).revision, revision);
    assert_eq!(
        receiver.try_iter().count(),
        1,
        "edit bursts coalesce to one bounded wake"
    );
    let instances = complete("!!shell-3", 9, None);
    assert_eq!(instances.len(), 10);
    assert!(
        instances
            .iter()
            .all(|candidate| candidate.replacement.starts_with("!!shell-3"))
    );
}

/// Merged results retain source labels and meaningful theme-styled descriptions
/// without decorative separators, while enforcing one shared page cap.
#[test]
fn remote_completion_merges_sources_with_one_result_bound() {
    let (cache, _receiver) = cache();
    for instance in ["first", "second"] {
        let context = Context {
            session_id: "session".parse().expect("session"),
            agent_id: None,
            request_id: instance.to_owned(),
        };
        let target = Target {
            instance: instance.parse().expect("instance"),
            nonce: instance.to_owned(),
        };
        {
            let mut state = cache.state.lock().expect(MUTEX_POISONED);
            state.draft = Some(Draft {
                generation: 0,
                buffer: "./".to_owned(),
                cursor: 2,
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
        assert!(
            cache.accept(
                context,
                target,
                (0..100)
                    .map(|index| PathCandidate {
                        value: format!("./file-{index:03}"),
                        directory: false,
                    })
                    .collect()
            )
        );
    }
    let state = cache.state.lock().expect(MUTEX_POISONED);
    assert_eq!(state.results.len(), 100);
    assert_eq!(state.results[0].description, "first file");
    assert_eq!(state.results[1].description, "second file");
    let view = tau_cli_term_raw::CompletionView {
        candidates: state.results[..2].to_vec(),
        selected: Some(0),
    };
    let theme = tau_themes::Theme::parse(
        r##"{
        styles: {
            "completion.label": { fg: "blue" },
            "completion.desc": { fg: "cyan" },
            "completion.selected": { fg: "yellow" },
        }
    }"##,
    )
    .expect("completion theme");
    let mut catalog = String::from(
        "# Actual path-completion menu samples\n\nAfter: candidates from the production path cache. Before: the same candidates with the baseline source/type description. Both use the native completion-menu renderer. Path dots and replacements stay unchanged.\n",
    );
    for width in [100, 36] {
        for before in [true, false] {
            let mut fixture = view.clone();
            if before {
                for candidate in &mut fixture.candidates {
                    candidate.description = candidate.description.replace(" file", " · file");
                }
            }
            let block = tau_cli_term::completion::render_menu_block(&fixture, &theme, width, 24);
            let rows = tau_term_screen::layout_block(&block, width);
            let text = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.ch)
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !before {
                assert!(!text.contains('·'));
                assert!(text.contains("./file-000"));
                let description = block
                    .content
                    .spans()
                    .iter()
                    .find(|span| span.text.contains("second file"))
                    .expect("unselected description");
                assert_eq!(
                    description.style,
                    tau_cli_term::resolve::resolve(&theme, tau_themes::names::COMPLETION_DESC)
                );
            }
            write!(
                &mut catalog,
                "\n## {} at {width} columns\n\n```text\n{text}\n```\n",
                if before { "Before" } else { "After" }
            )
            .expect("format catalog");
        }
    }
    if let Ok(directory) = std::env::var("TAU_TOOL_UI_FIXTURE_DIR") {
        std::fs::write(Path::new(&directory).join("path-completion.md"), catalog)
            .expect("write completion samples");
    }
}

/// Drafts and suffixes stay local; only the decoded cursor token is
/// transmitted.
#[test]
fn remote_completion_token_preserves_ranges_and_utf8() {
    for (buffer, cursor, decoded, before, after) in [
        (
            "!remote cat './hé llo' tail",
            "!remote cat './hé".len(),
            "./hé",
            "!remote cat ",
            " tail",
        ),
        (
            "open ./hé\\ llo tail",
            "open ./hé\\ l".len(),
            "./hé l",
            "open ",
            " tail",
        ),
        ("cat ~/path", "cat ~/path".len(), "~/path", "cat ", ""),
        (
            r#"cat "./a\b""#,
            r#"cat "./a\b""#.len(),
            r"./a\b",
            "cat ",
            "",
        ),
        (
            r#"cat "./a\$b""#,
            r#"cat "./a\$b""#.len(),
            "./a$b",
            "cat ",
            "",
        ),
        ("cat ./a\\\nb", "cat ./a\\\nb".len(), "./ab", "cat ", ""),
    ] {
        let parsed = token(buffer, cursor).expect("token");
        assert_eq!(
            (parsed.decoded.as_str(), parsed.before, parsed.after),
            (decoded, before, after)
        );
    }
    assert_eq!(quote("./hé llo'$"), "./hé\\ llo\\'\\$");
    assert!(token("é", 1).is_none());
}

/// Stale contexts, expiry, wrong agents/nonces and duplicates cannot populate
/// the current completion page or consume a genuine outstanding correlation.
#[test]
fn remote_completion_rejects_stale_correlations() {
    let (cache, _receiver) = cache();
    let context = Context {
        session_id: "session".parse().expect("session"),
        agent_id: Some("agent".parse().expect("agent")),
        request_id: "request".to_owned(),
    };
    let target = Target {
        instance: "remote".parse().expect("instance"),
        nonce: "process".to_owned(),
    };
    let prepare = |deadline| {
        let mut state = cache.state.lock().expect(MUTEX_POISONED);
        state.draft = Some(Draft {
            generation: 0,
            buffer: "open ./hé tail".to_owned(),
            cursor: "open ./hé".len(),
            agent: context.agent_id.clone(),
            targets: vec![target.clone()],
            fuzzy: false,
        });
        state.pending.insert(
            context.request_id.clone(),
            Pending {
                context: context.clone(),
                target: target.clone(),
                deadline,
            },
        );
    };
    let candidates = || {
        vec![PathCandidate {
            value: "./hé llo".to_owned(),
            directory: false,
        }]
    };
    prepare(Instant::now() + Duration::from_secs(1));
    let mut wrong = target.clone();
    wrong.nonce = "old".to_owned();
    assert!(!cache.accept(context.clone(), wrong, candidates()));
    assert!(
        cache
            .state
            .lock()
            .expect(MUTEX_POISONED)
            .pending
            .contains_key(&context.request_id)
    );
    prepare(Instant::now() - Duration::from_secs(1));
    assert!(!cache.accept(context.clone(), target.clone(), candidates()));
    prepare(Instant::now() + Duration::from_secs(1));
    let mut wrong = context.clone();
    wrong.agent_id = None;
    assert!(!cache.accept(wrong, target.clone(), candidates()));
    prepare(Instant::now() + Duration::from_secs(1));
    cache.invalidate();
    assert!(!cache.accept(context.clone(), target.clone(), candidates()));
    prepare(Instant::now() + Duration::from_secs(1));
    assert!(cache.accept(context.clone(), target.clone(), candidates()));
    assert!(!cache.accept(context, target, candidates()));
    let state = cache.state.lock().expect(MUTEX_POISONED);
    assert_eq!(state.results[0].replacement, "open ./hé\\ llo tail");
    assert_eq!(state.results[0].description, "remote file");
}
