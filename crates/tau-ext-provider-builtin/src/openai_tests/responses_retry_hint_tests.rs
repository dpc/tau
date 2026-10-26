use super::*;

/// Real HTTP errors and rejected upgrades carry their trusted lower bound into
/// scheduling, while usage-window estimates remain informational.
#[test]
fn responses_transport_hint_controls_scheduler_boundary_except_usage_window() {
    for transport in [
        tau_provider_responses::Transport::Sse,
        tau_provider_responses::Transport::Websocket,
    ] {
        for (code, expected_delay) in [
            ("rate_limit_exceeded", Duration::from_secs(3600)),
            ("usage_limit_reached", Duration::from_secs(17)),
        ] {
            let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("address");
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().expect("request");
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write timeout");
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).expect("request header");
                    headers.push(byte[0]);
                    assert!(headers.len() < 16_384, "bounded request header");
                }
                let headers = String::from_utf8(headers).expect("header text");
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|length| length.parse::<usize>().expect("body length"))
                    })
                    .unwrap_or(0);
                assert!(length < 1024 * 1024, "bounded test request");
                socket
                    .read_exact(&mut vec![0; length])
                    .expect("request body");
                let body = serde_json::json!({"error":{"code":code}}).to_string();
                write!(socket,
                    "HTTP/1.1 400 Rejected\r\nRetry-After: 3600\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()
                ).expect("rejection");
            });
            let outcome = tau_provider_responses::run_attempt(
                &prompt(),
                &tau_provider_responses::AttemptConfig {
                    base_url: format!("http://{address}"),
                    api_key: String::new(),
                    max_output_tokens: 0,
                    transport,
                    prompt_cache: None,
                },
                &tau_provider_responses::AttemptModel {
                    id: tau_proto::ModelName::new("test-model"),
                },
                &mut |_| {},
                &mut || false,
                &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
            );
            server.join().expect("server");
            let tau_provider_responses::AttemptOutcome::Retryable { decision, .. } = outcome else {
                panic!("structured transient must reach scheduler");
            };
            assert_eq!(decision.retry_after, Some(Duration::from_secs(3600)));
            let now = Instant::now();
            let hint = scheduler_retry_hint(decision.class, decision.retry_after)
                .unwrap_or(Duration::ZERO);
            assert_eq!(
                retry_common_due(now, Duration::from_secs(17), hint),
                now + expected_delay,
            );
        }
    }
}
