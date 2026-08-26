//! Tests for agent discovery behavior.

use super::*;

#[test]
fn discover_agents_files_walks_ancestor_chain_in_order() {
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path();
    let nested = root.join("pkg/src");
    fs::create_dir_all(&nested).expect("mkdir");

    let root_agents = root.join("AGENTS.md");
    let root_extra_agents = root.join("AGENTS.extra.md");
    let ignored_agents = root.join("AGENTS.txt");
    let ignored_dir_agents = root.join("AGENTS.dir.md");
    let pkg_agents = root.join("pkg").join("AGENTS.md");
    let pkg_extra_agents = root.join("pkg").join("AGENTS.zeta.md");
    let empty_agents = root.join("pkg").join("src").join("AGENTS.md");

    fs::write(&root_agents, "# Root\n- rule one\n").expect("write root");
    fs::write(&root_extra_agents, "# Root extra\n- rule extra\n").expect("write root extra");
    fs::write(&ignored_agents, "# Ignored\n").expect("write ignored");
    fs::create_dir(&ignored_dir_agents).expect("create ignored agents dir");
    fs::write(&pkg_agents, "# Package\n- rule two\n").expect("write pkg");
    fs::write(&pkg_extra_agents, "# Package extra\n- rule zeta\n").expect("write pkg extra");
    fs::write(&empty_agents, "   \n").expect("write empty");

    let discovered = discover_agents_files_from(&nested);
    assert_eq!(discovered.len(), 4);
    assert_eq!(
        discovered[0].file_path,
        root_agents.canonicalize().expect("canonical root")
    );
    assert_eq!(
        discovered[1].file_path,
        root_extra_agents
            .canonicalize()
            .expect("canonical root extra")
    );
    assert_eq!(
        discovered[2].file_path,
        pkg_agents.canonicalize().expect("canonical pkg")
    );
    assert_eq!(
        discovered[3].file_path,
        pkg_extra_agents
            .canonicalize()
            .expect("canonical pkg extra")
    );
    assert!(discovered[0].content.contains("rule one"));
    assert!(discovered[1].content.contains("rule extra"));
    assert!(discovered[2].content.contains("rule two"));
    assert!(discovered[3].content.contains("rule zeta"));
}

#[test]
fn discover_agents_files_follows_symlinked_candidates() {
    // AGENTS.md files are trusted prompt input. Tau follows symlinks here so
    // project-local and dotfile-managed instruction layouts behave like ordinary
    // filesystem reads.
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path().join("repo");
    fs::create_dir_all(&root).expect("mkdir");
    let shared = tempdir.path().join("shared.AGENTS.md");
    fs::write(&shared, "# Shared\n- linked rule\n").expect("write shared agents");
    fs::write(root.join("AGENTS.good.md"), "# Good\n").expect("write good agents");
    symlink(&shared, root.join("AGENTS.md")).expect("symlink agents");

    let discovered = discover_agents_files_from_roots(vec![root]);
    assert_eq!(discovered.len(), 2);
    assert_eq!(
        discovered[0].file_path,
        shared.canonicalize().expect("canonical shared")
    );
    assert!(discovered[0].content.contains("linked rule"));
    assert!(discovered[1].file_path.ends_with("AGENTS.good.md"));
}

/// Ensures symlinked project `.agents.local` roots are followed like normal
/// trusted instruction directories.
#[test]
fn discover_agents_files_follows_symlinked_agent_roots() {
    let tempdir = TempDir::new().expect("tempdir");
    let repo = tempdir.path().join("repo");
    let shared = tempdir.path().join("shared-agents");
    fs::create_dir_all(&repo).expect("mkdir repo");
    fs::create_dir_all(&shared).expect("mkdir shared");
    fs::write(shared.join("AGENTS.md"), "# Shared root\n- linked root\n")
        .expect("write shared agents");
    symlink(&shared, repo.join(".agents.local")).expect("symlink agents local");

    let discovered = discover_agents_files_from_roots(vec![repo.join(".agents.local")]);
    assert_eq!(discovered.len(), 1);
    assert!(discovered[0].content.contains("linked root"));
}

#[test]
fn discover_agents_files_skips_oversized_candidates() {
    // Session-start AGENTS loading must have its own input cap; output caps on
    // later tool calls do not protect the implicit instruction payload.
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path();
    fs::write(root.join("AGENTS.md"), "x".repeat(1024 * 1024 + 1)).expect("write huge agents");
    fs::write(root.join("AGENTS.ok.md"), "# Ok\n").expect("write ok agents");

    let discovered = discover_agents_files_from_roots(vec![root.to_path_buf()]);
    assert_eq!(discovered.len(), 1);
    assert!(discovered[0].file_path.ends_with("AGENTS.ok.md"));
}

#[test]
fn discover_agents_files_includes_local_agent_dirs_after_regular_paths() {
    let tempdir = TempDir::new().expect("tempdir");
    let repo = tempdir.path().join("repo");
    let nested = repo.join("pkg");
    fs::create_dir_all(nested.join(".agents.local")).expect("nested local agents dir");

    let repo_agents = repo.join("AGENTS.md");
    let repo_local_agents = repo.join(".agents.local").join("AGENTS.md");
    let nested_agents = nested.join("AGENTS.md");
    let nested_local_agents = nested.join(".agents.local").join("AGENTS.md");
    fs::create_dir_all(repo.join(".agents.local")).expect("repo local agents dir");
    fs::write(&repo_agents, "# Repo\n").expect("write repo");
    fs::write(&repo_local_agents, "# Repo local\n").expect("write repo local");
    fs::write(&nested_agents, "# Nested\n").expect("write nested");
    fs::write(&nested_local_agents, "# Nested local\n").expect("write nested local");

    let discovered = discover_agents_files_from(&nested);
    let paths: Vec<PathBuf> = discovered.iter().map(|f| f.file_path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            repo_agents.canonicalize().expect("canonical repo"),
            repo_local_agents
                .canonicalize()
                .expect("canonical repo local"),
            nested_agents.canonicalize().expect("canonical nested"),
            nested_local_agents
                .canonicalize()
                .expect("canonical nested local"),
        ]
    );
}

#[test]
fn project_scoped_skills_are_advertised_by_default() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path().join("repo");
    let home = temp.path().join("home");
    let project_skill_dir = cwd.join(".agents").join("skills").join("project-skill");
    let user_skill_dir = home.join(".agents").join("skills").join("user-skill");
    fs::create_dir_all(&project_skill_dir).expect("create project skill dir");
    fs::create_dir_all(&user_skill_dir).expect("create user skill dir");
    let project_hidden_dir = cwd
        .join(".agents")
        .join("skills")
        .join("project-hidden-skill");
    fs::create_dir_all(&project_hidden_dir).expect("create hidden project skill dir");
    fs::write(
        project_skill_dir.join("SKILL.md"),
        "---\nname: project-skill\ndescription: Project skill\n---\nbody\n",
    )
    .expect("write project skill");
    fs::write(
        project_hidden_dir.join("SKILL.md"),
        "---\nname: project-hidden-skill\ndescription: Hidden project skill\nadvertise: false\n---\nbody\n",
    )
    .expect("write hidden project skill");
    fs::write(
        user_skill_dir.join("SKILL.md"),
        "---\nname: user-skill\ndescription: User skill\n---\nbody\n",
    )
    .expect("write user skill");

    let result =
        tau_skills::load_skills_from_skill_dirs(&session_skill_dirs(Some(cwd), Some(home)));
    let project_skill = result
        .skills
        .iter()
        .find(|skill| skill.name == "project-skill")
        .expect("project skill");
    let user_skill = result
        .skills
        .iter()
        .find(|skill| skill.name == "user-skill")
        .expect("user skill");
    let project_hidden_skill = result
        .skills
        .iter()
        .find(|skill| skill.name == "project-hidden-skill")
        .expect("hidden project skill");

    assert!(project_skill.add_to_prompt);
    assert!(!project_hidden_skill.add_to_prompt);
    assert!(!user_skill.add_to_prompt);
}

#[test]
fn session_agent_loaded_emits_ready_after_agent_context_publish() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            ephemeral: false,
        }))
        .expect("request");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                session_id: Some(
                    "s1".parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            },
        )))
        .expect("agent replay boundary");
    writer.flush().expect("flush");
    let metadata = loop {
        let event = reader.read_event().expect("read").expect("metadata event");
        if let Event::AgentMetadataSetRequest(metadata) = event {
            break metadata;
        }
    };
    writer
        .write_event(&Event::AgentMetadataSet(metadata))
        .expect("commit metadata");
    writer.flush().expect("flush metadata");

    let mut saw_cwd_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("context event");
        match event {
            Event::ExtAgentContextPublish(publish) if publish.key.as_ref() == "workdir" => {
                saw_cwd_context = true;
            }
            Event::ExtensionContextReady(ready) => {
                assert!(saw_cwd_context, "ready must follow cwd context publish");
                assert_eq!(ready.session_id, "s1");
                assert_eq!(ready.agent_id.as_str(), "agent-1");
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Agent load publishes its correlated snapshot before requesting workdir
/// metadata and waits for the committed metadata before context readiness.
#[test]
fn session_agent_loaded_publishes_current_directory_context_for_agent() {
    // Agent context is the structured source used by the shell cwd prompt
    // fragment; it must be keyed by durable agent, not by session.
    let cwd = std::env::current_dir().expect("current dir");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let output = Output::channel(tx);
    let cwd_state = CwdState::new();

    dispatch_session_agent_loaded(
        tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: tau_proto::SessionId::parse("session-1")
                .expect("known-safe SessionId must be valid"),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            ephemeral: false,
        },
        &output,
        &cwd_state,
        false,
        DiscoverySourcePolicy::Environment,
    )
    .expect("agent load");

    loop {
        let message = rx.recv().expect("discovery snapshot");
        if matches!(
            message,
            HarnessInputMessage::Emit(ref emit)
                if matches!(emit.event.as_ref(),
                    Event::ExtensionAgentDiscoverySnapshotDeclared(declared)
                        if declared.agent_id.as_str() == "agent-1")
        ) {
            break;
        }
    }
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("cwd metadata publish") else {
        panic!("expected cwd metadata publish");
    };
    assert!(!emit.persist, "metadata mutations are transient requests");
    let Event::AgentMetadataSetRequest(metadata) = *emit.event else {
        panic!("expected cwd metadata publish");
    };
    assert_eq!(metadata.key.as_str(), "ext_core-shell_cwd");
    assert!(metadata.inheritable);
    assert!(
        rx.try_recv().is_err(),
        "context waits for committed metadata"
    );

    cwd_state.set(
        metadata.agent_id.clone(),
        PathBuf::from(cwd.display().to_string()),
    );
    let context = cwd_context_event(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        metadata.agent_id,
        tau_proto::AgentInitializationId::parse("init-1").expect("test identifier must be valid"),
        &cwd,
        &cwd_state,
    );
    let Event::ExtAgentContextPublish(publish) = context else {
        panic!("expected cwd agent context publish");
    };
    assert_eq!(publish.agent_id.as_ref(), "agent-1");
    assert_eq!(publish.key.as_ref(), "workdir");
    assert_eq!(publish.value.0["path"], cwd.display().to_string());
    assert_eq!(publish.value.0["label"], "default");
    assert_eq!(publish.value.0["status"], "available");
}

/// Ensures session-only skill collision notices cannot be copied into a
/// per-agent discovery transaction ahead of its mandatory snapshot.
#[test]
fn per_agent_discovery_excludes_session_collision_diagnostics() {
    let mut diagnostics = Vec::new();
    push_skill_diagnostic_requests(
        &mut diagnostics,
        vec![tau_skills::SkillDiagnostic {
            path: PathBuf::from("collision/SKILL.md"),
            kind: tau_skills::DiagnosticKind::Collision,
            message: "name collision".to_owned(),
        }],
    );
    let scan = DiscoveryScan {
        snapshot: tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
            session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
            skills: Vec::new(),
            agents_files: Vec::new(),
        },
        diagnostics,
    };
    assert_eq!(
        scan.diagnostics.len(),
        1,
        "fixture must contain a collision"
    );
    let (tx, rx) = path_std_sync::mpsc::channel();
    publish_agent_discovery_scan(
        scan,
        tau_proto::AgentId::parse("agent-1").expect("agent id"),
        tau_proto::AgentInitializationId::parse("init-1").expect("initialization id"),
        &Output::channel(tx),
    )
    .expect("publish agent discovery");

    let message = rx.recv().expect("agent snapshot");
    let HarnessInputMessage::Emit(emit) = message else {
        panic!("agent discovery must be one emitted event");
    };
    assert!(matches!(
        emit.event.as_ref(),
        Event::ExtensionAgentDiscoverySnapshotDeclared(event)
            if event.agent_id.as_str() == "agent-1"
    ));
    assert!(
        rx.try_recv().is_err(),
        "per-agent publication must not emit session diagnostics"
    );
}

/// Ensures user instruction roots still load before project instructions, while
/// preferring the XDG user directories over legacy `~/.agents` roots.
#[test]
fn user_agents_roots_prefer_config_agents_before_legacy_home_agents() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    let repo = tempdir.path().join("repo");
    fs::create_dir_all(home.join(".agents")).expect("home agents dir");
    fs::create_dir_all(home.join(".agents.local")).expect("home local agents dir");
    fs::create_dir_all(home.join(".config").join("agents")).expect("config agents dir");
    fs::create_dir_all(home.join(".config").join("agents.local")).expect("config local agents dir");
    fs::create_dir_all(repo.join("pkg")).expect("repo pkg dir");

    let config_agents = home.join(".config").join("agents").join("AGENTS.md");
    let config_local_agents = home.join(".config").join("agents.local").join("AGENTS.md");
    let legacy_agents = home.join(".agents").join("AGENTS.md");
    let legacy_local_agents = home.join(".agents.local").join("AGENTS.md");
    let repo_agents = repo.join("AGENTS.md");
    let pkg_agents = repo.join("pkg").join("AGENTS.md");
    fs::write(&config_agents, "# Home config\n- preferred personal rule\n")
        .expect("write config home");
    fs::write(&config_local_agents, "# Home config local\n").expect("write config local home");
    fs::write(&legacy_agents, "# Home legacy\n- legacy personal rule\n")
        .expect("write legacy home");
    fs::write(&legacy_local_agents, "# Home legacy local\n").expect("write legacy local home");
    fs::write(&repo_agents, "# Repo\n- repo rule\n").expect("write repo");
    fs::write(&pkg_agents, "# Package\n- package rule\n").expect("write pkg");

    let mut roots = user_agents_roots(&home);
    roots.push(repo.clone());
    roots.push(repo.join("pkg"));
    let discovered = discover_agents_files_from_roots(roots);

    let paths: Vec<PathBuf> = discovered.iter().map(|f| f.file_path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            config_agents.canonicalize().expect("canonical config home"),
            config_local_agents
                .canonicalize()
                .expect("canonical config local home"),
            legacy_agents.canonicalize().expect("canonical legacy home"),
            legacy_local_agents
                .canonicalize()
                .expect("canonical legacy local home"),
            repo_agents.canonicalize().expect("canonical repo"),
            pkg_agents.canonicalize().expect("canonical pkg"),
        ]
    );
}

/// Ensures user skill roots keep project roots first, then assign XDG user
/// roots higher collision precedence than legacy `~/.agents` roots.
#[test]
fn session_skill_dirs_include_config_agents() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir_all(cwd.join(".agents").join("skills")).expect("cwd agents skills");
    fs::create_dir_all(cwd.join(".agents.local").join("skills")).expect("cwd local agents skills");

    let dirs = session_skill_dirs(Some(cwd.clone()), Some(home.clone()));
    let paths: Vec<_> = dirs.iter().map(|dir| dir.path.clone()).collect();
    let prompt_defaults: Vec<_> = dirs
        .iter()
        .map(|dir| dir.add_to_prompt_by_default)
        .collect();
    let source_precedence: Vec<_> = dirs.iter().map(|dir| dir.source_precedence).collect();

    assert_eq!(
        paths,
        vec![
            cwd.join(".agents").join("skills"),
            cwd.join(".agents.local").join("skills"),
            home.join(".config").join("agents").join("skills"),
            home.join(".config").join("agents.local").join("skills"),
            home.join(".agents").join("skills"),
            home.join(".agents.local").join("skills"),
        ]
    );
    assert_eq!(
        prompt_defaults,
        vec![true, true, false, false, false, false]
    );
    assert_eq!(
        source_precedence,
        vec![None, None, Some(0), Some(0), Some(1), Some(1)]
    );
}

/// Ensures only existing project skill roots from the ancestor chain are
/// advertised by default, and user roots are appended afterward.
#[test]
fn session_skill_dirs_include_existing_project_ancestors() {
    let temp = TempDir::new().expect("tempdir");
    let repo = temp.path().join("repo");
    let pkg = repo.join("pkg");
    let cwd = pkg.join("src");
    let home = temp.path().join("home");
    let repo_skills = repo.join(".agents").join("skills");
    let pkg_local_skills = pkg.join(".agents.local").join("skills");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::create_dir_all(&repo_skills).expect("repo skills");
    fs::create_dir_all(&pkg_local_skills).expect("pkg local skills");

    let dirs = session_skill_dirs(Some(cwd), Some(home.clone()));
    let paths: Vec<_> = dirs.iter().map(|dir| dir.path.clone()).collect();

    assert_eq!(
        paths,
        vec![
            repo_skills,
            pkg_local_skills,
            home.join(".config").join("agents").join("skills"),
            home.join(".config").join("agents.local").join("skills"),
            home.join(".agents").join("skills"),
            home.join(".agents.local").join("skills"),
        ]
    );
}

/// Ensures a home directory that contains the current working directory is not
/// treated as a project-skill root, including the preferred XDG user roots.
#[test]
fn session_skill_dirs_do_not_treat_home_agents_as_project_skills() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let cwd = home.join("repo");
    let home_skills = home.join(".agents").join("skills");
    let repo_skills = cwd.join(".agents").join("skills");
    fs::create_dir_all(&home_skills).expect("home skills");
    fs::create_dir_all(&repo_skills).expect("repo skills");

    let dirs = session_skill_dirs(Some(cwd), Some(home.clone()));
    let project_defaults: Vec<_> = dirs
        .iter()
        .map(|dir| (dir.path.clone(), dir.add_to_prompt_by_default))
        .collect();

    assert_eq!(
        project_defaults,
        vec![
            (repo_skills, true),
            (home.join(".config").join("agents").join("skills"), false),
            (
                home.join(".config").join("agents.local").join("skills"),
                false,
            ),
            (home_skills, false),
            (home.join(".agents.local").join("skills"), false),
        ]
    );
}

#[test]
fn skill_diagnostics_use_extension_notice_requests() {
    let temp = TempDir::new().expect("tempdir");
    let skills_dir = temp.path().join(".agents").join("skills");
    let skill_dir = skills_dir.join("bad-skill");
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: bad skill\ndescription: bad skill\n---\n\n# Bad\n",
    )
    .expect("write skill");

    let result = tau_skills::load_skills_from_dirs(&[skills_dir]);
    assert!(result.skills.is_empty());

    let mut messages = Vec::new();
    push_skill_diagnostic_requests(&mut messages, result.diagnostics);

    let skipped = messages.iter().find_map(|message| match message {
        HarnessInputMessage::ExtensionNoticeRequest(request)
            if request.message.contains("skill skipped:") =>
        {
            Some(request)
        }
        _ => None,
    });
    let Some(skipped_request) = skipped else {
        panic!("expected skipped skill notice request, got {messages:?}");
    };
    assert_eq!(skipped_request.level, tau_proto::NoticeLevel::Warning);
    assert!(skipped_request.message.contains("bad-skill/SKILL.md"));
    assert!(
        skipped_request
            .message
            .contains("name contains invalid characters")
    );
}

/// Ensures ext-shell keeps the expected notice severity for each skill-loader
/// diagnostic kind: soft warnings are informational, expected collisions are
/// trace-only, and skipped skills stay visible warnings.
#[test]
fn skill_diagnostics_map_expected_notice_levels() {
    let mut messages = Vec::new();
    push_skill_diagnostic_requests(
        &mut messages,
        vec![
            tau_skills::SkillDiagnostic {
                path: PathBuf::from("warn/SKILL.md"),
                kind: tau_skills::DiagnosticKind::Warning,
                message: "soft warning".to_owned(),
            },
            tau_skills::SkillDiagnostic {
                path: PathBuf::from("collision/SKILL.md"),
                kind: tau_skills::DiagnosticKind::Collision,
                message: "name collision".to_owned(),
            },
            tau_skills::SkillDiagnostic {
                path: PathBuf::from("skipped/SKILL.md"),
                kind: tau_skills::DiagnosticKind::Skipped,
                message: "fatal skip".to_owned(),
            },
        ],
    );

    let notices = messages
        .iter()
        .map(|message| match message {
            HarnessInputMessage::ExtensionNoticeRequest(request) => request,
            other => panic!("expected extension notice request, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let warning_request = notices
        .iter()
        .find(|request| request.message.contains("skill warning:"))
        .expect("warning diagnostic");
    assert_eq!(warning_request.level, tau_proto::NoticeLevel::Info);

    let collision_request = notices
        .iter()
        .find(|request| request.message.contains("skill collision:"))
        .expect("collision diagnostic");
    assert_eq!(collision_request.level, tau_proto::NoticeLevel::Trace);

    let skipped_request = notices
        .iter()
        .find(|request| request.message.contains("skill skipped:"))
        .expect("skipped diagnostic");
    assert_eq!(skipped_request.level, tau_proto::NoticeLevel::Warning);
}
