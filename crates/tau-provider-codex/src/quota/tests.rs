use std::{io as path_std_io, net as path_std_net, sync as path_std_sync};

use super::*;

/// The full account read uses the isolated `/wham/usage` endpoint and sends
/// bearer/account headers while returning only normalized quota facts.
#[test]
fn full_fetch_uses_expected_endpoint_and_auth_headers() {
    let listener = path_std_net::TcpListener::bind(("127.0.0.1", 0)).expect("bind usage server");
    let address = listener.local_addr().expect("usage server address");
    let (request_tx, request_rx) = path_std_sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept usage request");
        let mut request = vec![0_u8; 8192];
        let read = path_std_io::Read::read(&mut stream, &mut request).expect("read usage request");
        request.truncate(read);
        request_tx
            .send(String::from_utf8(request).expect("HTTP request is UTF-8"))
            .expect("send captured request");
        let body = r#"{"rate_limit":{"secondary_window":{"used_percent":42,"limit_window_seconds":604800,"reset_after_seconds":300000,"reset_at":2000000000}}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        path_std_io::Write::write_all(&mut stream, response.as_bytes())
            .expect("write usage response");
    });
    let snapshot = fetch_usage(
        &format!("http://{address}/backend-api/"),
        "test-token",
        Some("account-123"),
        &crate::test_network_policy(),
    )
    .expect("fetch usage");
    let request = request_rx.recv().expect("captured usage request");
    assert!(request.starts_with("GET /backend-api/wham/usage HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-token\r\n")
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: account-123\r\n")
    );
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].used_basis_points, 4_200);
}

/// Full snapshots retain primary/secondary identity, relative and absolute
/// timing, and independent additional pools while ignoring unrelated account
/// metadata.
#[test]
fn full_usage_normalizes_all_supported_windows() {
    let snapshot = parse_full_usage_json(
        r#"{
          "plan_type":"pro",
          "rate_limit":{
            "primary_window":{"used_percent":12.5,"limit_window_seconds":18000,"reset_after_seconds":9000,"reset_at":20000},
            "secondary_window":{"used_percent":40,"limit_window_seconds":604800,"reset_after_seconds":302400,"reset_at":700000}
          },
          "credits":{"balance":"secret"},
          "additional_rate_limits":[{
            "metered_feature":"Codex-Fast",
            "limit_name":"provider prose",
            "rate_limit":{"secondary_window":{"used_percent":90,"limit_window_seconds":604800,"reset_after_seconds":100,"reset_at":800000}}
          }]
        }"#,
    )
    .expect("valid quota test value");
    let mut windows = snapshot
        .windows
        .iter()
        .map(|window| {
            (
                window.limit_id.as_str(),
                window.window_id.as_str(),
                window.used_basis_points,
                window
                    .window_seconds
                    .map(tau_proto::QuotaWindowSeconds::get),
                window.remaining_seconds.map(tau_proto::SignedSeconds::get),
                window
                    .reset_at_unix_seconds
                    .map(tau_proto::UnixSeconds::get),
            )
        })
        .collect::<Vec<_>>();
    windows.sort_unstable();
    assert_eq!(
        windows,
        vec![
            (
                "codex",
                "primary",
                1_250,
                Some(18_000),
                Some(9_000),
                Some(20_000),
            ),
            (
                "codex",
                "secondary",
                4_000,
                Some(604_800),
                Some(302_400),
                Some(700_000),
            ),
            (
                "codex_fast",
                "secondary",
                9_000,
                Some(604_800),
                Some(100),
                Some(800_000),
            ),
        ]
    );
}

/// A malformed pool is rejected independently, and normalization collisions
/// reject both aliases instead of accidentally merging their quotas.
#[test]
fn malformed_and_colliding_additional_pools_fail_closed() {
    let snapshot = parse_full_usage_json(
        r#"{
          "rate_limit":{"primary_window":{"used_percent":150,"limit_window_seconds":1}},
          "additional_rate_limits":[
            {"metered_feature":"codex-fast","rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":604800}}},
            {"metered_feature":"codex_fast","rate_limit":{"primary_window":{"used_percent":2,"limit_window_seconds":604800}}},
            {"metered_feature":7,"rate_limit":"malformed"},
            {"metered_feature":"good","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":604800}}}
          ]
        }"#,
    )
    .expect("valid quota test value");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].limit_id.as_str(), "good");
    assert_eq!(snapshot.windows[0].used_basis_points, 0);
}

/// WebSocket and Lite-compatible `codex.rate_limits` events create the same
/// normalized sparse record and explicit route binding.
#[test]
fn websocket_event_normalizes_and_binds_named_pool() {
    let observation = parse_ws_event(
        r#"{"type":"codex.rate_limits","metered_limit_name":"codex_bengalfox","rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"reset_at":1700000000},"secondary":{"used_percent":45,"window_minutes":10080,"reset_at":1700600000}}}"#,
    )
    .expect("valid quota test value");
    assert_eq!(
        observation
            .active_limit_id
            .as_ref()
            .expect("valid quota test value")
            .as_str(),
        "codex_bengalfox"
    );
    assert_eq!(
        observation.binding_provenance,
        Some(tau_proto::ProviderQuotaBindingProvenance::TurnEvent)
    );
    let mut windows = observation
        .windows
        .iter()
        .map(|window| {
            (
                window.limit_id.as_str(),
                window.window_id.as_str(),
                window.used_basis_points,
                window
                    .window_seconds
                    .map(tau_proto::QuotaWindowSeconds::get),
                window.remaining_seconds.map(tau_proto::SignedSeconds::get),
                window
                    .reset_at_unix_seconds
                    .map(tau_proto::UnixSeconds::get),
            )
        })
        .collect::<Vec<_>>();
    windows.sort_unstable();
    assert_eq!(
        windows,
        vec![
            (
                "codex_bengalfox",
                "primary",
                1_250,
                Some(18_000),
                None,
                Some(1_700_000_000),
            ),
            (
                "codex_bengalfox",
                "secondary",
                4_500,
                Some(604_800),
                None,
                Some(1_700_600_000),
            ),
        ]
    );
}

/// The ordinary official Codex WebSocket shape omits both optional pool-name
/// fields, which authoritatively identifies the default `codex` pool for that
/// exact turn rather than inferring from account pool enumeration.
#[test]
fn websocket_event_binds_nameless_official_shape_to_default_pool() {
    let observation = parse_ws_event(
        r#"{"type":"codex.rate_limits","plan_type":"plus","rate_limits":{"allowed":true,"limit_reached":false,"secondary":{"used_percent":45,"window_minutes":10080,"reset_at":1700600000}},"code_review_rate_limits":null,"credits":{"has_credits":true,"unlimited":false,"balance":"123"}}"#,
    )
    .expect("official nameless quota event");
    assert_eq!(
        observation
            .active_limit_id
            .expect("default-pool binding")
            .as_str(),
        "codex"
    );
    assert_eq!(
        observation.binding_provenance,
        Some(tau_proto::ProviderQuotaBindingProvenance::TurnEvent)
    );
    assert_eq!(observation.windows.len(), 1);
    assert_eq!(
        observation.windows[0].window_seconds,
        Some(tau_proto::QuotaWindowSeconds::new(604_800))
    );
}

/// A present malformed explicit pool remains external untrusted data and must
/// not be reinterpreted as the otherwise-valid nameless default-pool contract.
#[test]
fn websocket_event_rejects_malformed_present_pool() {
    assert!(
        parse_ws_event(
            r#"{"type":"codex.rate_limits","metered_limit_name":"bad pool","rate_limits":{}}"#
        )
        .is_none()
    );
    assert!(
        parse_ws_event(
            r#"{"type":"codex.rate_limits","metered_limit_name":"bad pool","limit_name":"codex","rate_limits":{}}"#
        )
        .is_none()
    );
}

/// The legacy `limit_name` field remains an authoritative explicit fallback
/// when the preferred `metered_limit_name` field is absent.
#[test]
fn websocket_event_binds_valid_legacy_limit_name() {
    let observation = parse_ws_event(
        r#"{"type":"codex.rate_limits","limit_name":"codex_bengalfox","rate_limits":{}}"#,
    )
    .expect("valid legacy named event");
    assert_eq!(
        observation
            .active_limit_id
            .expect("legacy binding")
            .as_str(),
        "codex_bengalfox"
    );
}

/// Pool-field precedence distinguishes official null/absence from malformed
/// non-null external data and never lets an invalid preferred value fall
/// through to a lower-authority field.
#[test]
fn websocket_event_pool_field_precedence_is_fail_closed() {
    let cases = [
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":null,"limit_name":"codex_legacy"}"#,
            Some("codex_legacy"),
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":null,"limit_name":null}"#,
            Some("codex"),
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"codex_preferred","limit_name":"codex_legacy"}"#,
            Some("codex_preferred"),
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"codex_preferred","limit_name":""}"#,
            None,
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"codex_preferred","limit_name":"  "}"#,
            None,
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"codex_preferred","limit_name":"bad pool"}"#,
            None,
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"codex_preferred","limit_name":7}"#,
            None,
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"","limit_name":"codex_legacy"}"#,
            None,
        ),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":"  ","limit_name":"codex_legacy"}"#,
            None,
        ),
        (r#"{"type":"codex.rate_limits","limit_name":""}"#, None),
        (r#"{"type":"codex.rate_limits","limit_name":"  "}"#, None),
        (
            r#"{"type":"codex.rate_limits","metered_limit_name":7,"limit_name":"codex_legacy"}"#,
            None,
        ),
        (
            r#"{"type":"codex.rate_limits","limit_name":{"pool":"codex"}}"#,
            None,
        ),
    ];
    for (body, expected) in cases {
        let actual = parse_ws_event(body)
            .and_then(|observation| observation.active_limit_id)
            .map(|limit_id| limit_id.to_string());
        assert_eq!(actual.as_deref(), expected, "{body}");
    }
}

/// A full response that cannot fit the protocol window bound is rejected
/// atomically rather than truncated and mislabeled as complete.
#[test]
fn oversized_full_snapshot_is_rejected_atomically() {
    let additional = (0..17)
        .map(|index| {
            format!(
                r#"{{"metered_feature":"pool_{index}","rate_limit":{{"primary_window":{{"used_percent":1,"limit_window_seconds":18000}},"secondary_window":{{"used_percent":2,"limit_window_seconds":604800}}}}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(r#"{{"additional_rate_limits":[{additional}]}}"#);
    assert!(parse_full_usage_json(&body).is_err());
}

/// Non-finite and materially out-of-range percentages never cross the provider
/// boundary, while the documented half-point rounding tolerance is clamped.
#[test]
fn percentage_validation_accepts_only_small_rounding_error() {
    for invalid in ["101", "-1"] {
        let body = format!(
            r#"{{"rate_limit":{{"primary_window":{{"used_percent":{invalid},"limit_window_seconds":604800}}}}}}"#
        );
        assert!(
            parse_full_usage_json(&body)
                .expect("valid quota test value")
                .windows
                .is_empty()
        );
    }
    let snapshot = parse_full_usage_json(
        r#"{"rate_limit":{"primary_window":{"used_percent":100.5,"limit_window_seconds":604800}}}"#,
    )
    .expect("valid quota test value");
    assert_eq!(snapshot.windows[0].used_basis_points, 10_000);
}
