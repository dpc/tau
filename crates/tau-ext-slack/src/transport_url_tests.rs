//! Regression coverage for exact Slack transport URL identities.

use super::transport_url::{SlackApiBaseUrl, SlackSocketUrl};

/// API endpoint validation preserves current diagnostics and their
/// precedence.
#[test]
fn api_base_validation_preserves_exact_errors_and_precedence() {
    let cases = [
        ("", "slack `api_base` must not be empty"),
        (
            "not a URL",
            "slack `api_base` must be a valid URL: relative URL without a base",
        ),
        (
            "ftp://user@example.com/api?query#fragment",
            "slack `api_base` must not include userinfo",
        ),
        (
            "ftp://example.com/api?query#fragment",
            "slack `api_base` must not include query or fragment",
        ),
        (
            "http://example.com/api",
            "slack `api_base` may use http only for loopback hosts",
        ),
        (
            "ftp://example.com/api",
            "slack `api_base` must use https, or http for loopback tests",
        ),
    ];
    for (raw, expected) in cases {
        let error = match SlackApiBaseUrl::parse_exact(raw.to_owned()) {
            Ok(_) => panic!("must reject API base"),
            Err(error) => error,
        };
        assert_eq!(error, expected);
    }
}

/// API method construction retains spelling instead of applying URL
/// joining.
#[test]
fn api_base_retains_exact_bytes_for_method_urls() {
    for raw in [
        "https://EXAMPLE.com:443/a%2Fb",
        "https://example.com:8443/api//v1",
        "http://localhost:80/a/../b",
        "http://127.0.0.1:8080/api",
        "http://[::1]:8080/api",
    ] {
        let base = SlackApiBaseUrl::parse_exact(raw.to_owned()).expect("valid API base");
        assert_eq!(base.raw(), raw);
        assert_eq!(base.method_url("auth.test"), format!("{raw}/auth.test"));
    }

    let base =
        SlackApiBaseUrl::parse_exact("https://slack.com/api".to_owned()).expect("valid base");
    for method in [
        "apps.connections.open",
        "auth.test",
        "users.info",
        "chat.postMessage",
        "reactions.add",
        "reactions.remove",
    ] {
        assert_eq!(
            base.method_url(method),
            format!("https://slack.com/api/{method}")
        );
    }
}

/// Socket validation preserves the existing grammar, errors, and
/// precedence.
#[test]
fn socket_url_validation_preserves_exact_errors_and_precedence() {
    let cases = [
        (
            "",
            "Slack Socket Mode URL is invalid: relative URL without a base",
        ),
        (
            "not a URL",
            "Slack Socket Mode URL is invalid: relative URL without a base",
        ),
        (
            "ftp://user@example.com/socket",
            "Slack Socket Mode URL must not include userinfo",
        ),
        (
            "ws://example.com/socket",
            "Slack Socket Mode URL may use ws only for loopback hosts",
        ),
        (
            "ftp://example.com/socket",
            "Slack Socket Mode URL must use wss, or ws for loopback tests",
        ),
    ];
    for (raw, expected) in cases {
        let error = match SlackSocketUrl::parse_exact(raw.to_owned()) {
            Ok(_) => panic!("must reject Socket URL"),
            Err(error) => error,
        };
        assert_eq!(error, expected);
    }
}

/// Provider ticket path, query, and fragment bytes reach the connector
/// intact.
#[test]
fn socket_url_retains_exact_provider_ticket_bytes() {
    for raw in [
        "wss://wss-primary.slack.com/link/?ticket=a%2Fb&x=1#fragment",
        "ws://localhost:8080/socket/%2e%2e?ticket=a+b#suffix",
        "ws://127.0.0.1:8080/socket?ticket=secret",
        "ws://[::1]:8080/socket?ticket=secret",
    ] {
        let socket = SlackSocketUrl::parse_exact(raw.to_owned()).expect("valid Socket URL");
        assert_eq!(socket.raw(), raw);
    }
}
