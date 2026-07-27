use super::*;

#[test]
fn format_session_entry_tree_preview_shows_grouped_tool_results() {
    let result = AgentEntry::ToolResults {
        items: vec![tau_proto::ToolResultItem {
            call_id: "call_ugly".into(),
            tool_type: tau_proto::ToolType::Function,
            status: tau_proto::ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("hello".to_owned())),
            provider_content: Vec::new(),
        }],
    };
    assert_eq!(
        format_session_entry(&result),
        "tool.result call_ugly -> hello"
    );

    let multibyte_result = AgentEntry::ToolResults {
        items: vec![tau_proto::ToolResultItem {
            call_id: "call_utf8".into(),
            tool_type: tau_proto::ToolType::Function,
            status: tau_proto::ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("é".repeat(81))),
            provider_content: Vec::new(),
        }],
    };
    let formatted = format_session_entry(&multibyte_result);
    assert!(formatted.ends_with("..."));
}

#[test]
fn session_lines_are_printable() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }

    let _ = send_daemon_message_with_trace(&sock, "s1", "hello").expect("ok");
    server.join().expect("join").expect("clean exit");

    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let sl = session_lines(&sessions_dir, "s1").expect("lines");
    assert!(sl.iter().any(|l| l.contains("loaded agent")));
    assert!(sl.iter().all(|l| !l.contains("user: hello")));
    let sll = session_list_lines(&sessions_dir).expect("list");
    assert!(
        sll.iter()
            .any(|l| l.starts_with("s1 (") && l.contains("loaded agent"))
    );
}

#[test]
fn empty_session_views() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    std::fs::create_dir_all(&sessions_dir).expect("mkdir");
    assert_eq!(
        session_list_lines(&sessions_dir).expect("ok"),
        vec!["no sessions"]
    );
    assert_eq!(
        session_lines(&sessions_dir, "x").expect("ok"),
        vec!["session x not found"]
    );
}
