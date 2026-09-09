use super::*;

/// Captures the complete HTTP response after the writer closes its connection.
fn response_bytes(step: ScriptStep) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let mut client = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read deadline");
    let (mut server, _) = listener.accept().expect("accept");
    write_scripted_response(&mut server, step).expect("write response");
    drop(server);
    let mut bytes = Vec::new();
    client.read_to_end(&mut bytes).expect("read response");
    bytes
}

/// Preserves the throttle's exact HTTP status, JSON payload and Retry-After
/// header.
#[test]
fn throttle_response_preserves_wire_bytes() {
    let body = r#"{"error":{"code":"rate_limit_exceeded","type":"rate_limit_exceeded","message":"fixture throttle"}}"#;
    let expected = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 86400\r\n\r\n{body}",
        body.len(),
    );
    assert_eq!(response_bytes(ScriptStep::Throttle), expected.as_bytes());
}

/// Preserves SSE ordering and UTF-8 byte length without adding retry headers.
#[test]
fn qwen_final_response_preserves_wire_bytes() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"final plan\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Qwen complete ✓\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":101,\"completion_tokens\":17,\"total_tokens\":118}}\n\n",
        "data: [DONE]\n\n",
    );
    let expected = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    assert_eq!(response_bytes(ScriptStep::QwenFinal), expected.as_bytes());
}
