//! Real bounded loopback OAuth/JWKS exchange using public fixture keys.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use tokio::runtime::Builder;

use super::*;
use crate::authorization::Authorization;

/// Read a complete finite HTTP request without assuming TCP read boundaries.
fn request(stream: &mut std::net::TcpStream) -> (String, String) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read deadline");
    let mut bytes = Vec::new();
    let mut one = [0; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32 * 1024);
        stream.read_exact(&mut one).expect("header");
        bytes.push(one[0]);
    }
    let headers = String::from_utf8(bytes).expect("headers");
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("length"))
        })
        .unwrap_or(0);
    assert!(length < 64 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("body");
    (
        headers.lines().next().expect("request line").to_owned(),
        String::from_utf8(body).expect("form"),
    )
}

/// The issued client, exact resource and rotating token are preserved through
/// real HTTP exchange; a valid identity-only grant cannot authorize inference.
#[test]
fn code_exchange_and_refresh_use_issued_client_and_granted_scopes() {
    let now = now_ms().expect("clock");
    let (authorization, url) =
        Authorization::new(1455, "host", None, false, now).expect("authorization");
    let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let mut callback = url::Url::parse("http://127.0.0.1:1455/auth/callback").expect("callback");
    callback
        .query_pairs_mut()
        .append_pair("code", "code +/&")
        .append_pair("state", &params["state"])
        .append_pair("client_id", "oaiapp_test");
    let callback = authorization
        .callback(&callback, now)
        .expect("verified callback");
    let mut claims = crate::tests::claims(now);
    claims["nonce"] = params["nonce"].clone().into();
    let token = crate::tests::signed(&claims);
    let keys = serde_json::to_string(&crate::tests::keys()).expect("JWKS");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let issuer = format!("http://{}", listener.local_addr().expect("address"));
    let expected_redirect = params["redirect_uri"].clone();
    let peer = std::thread::spawn(move || {
        for index in 0..3 {
            let (mut socket, _) = listener.accept().expect("request");
            let (line, body) = request(&mut socket);
            let response = if index == 1 {
                assert!(line.starts_with("GET /.well-known/jwks.json "));
                keys.clone()
            } else {
                assert!(line.starts_with("POST /api/accounts/oauth/token "));
                let form: std::collections::BTreeMap<_, _> =
                    url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect();
                assert_eq!(form["client_id"], "oaiapp_test");
                assert_eq!(form["resource"], RESOURCE);
                assert!(!form.contains_key("client_secret"));
                if index == 0 {
                    assert_eq!(form["grant_type"], "authorization_code");
                    assert_eq!(form["redirect_uri"], expected_redirect);
                    assert_eq!(form["code"], "code +/&");
                    assert!(form.contains_key("code_verifier"));
                } else {
                    assert_eq!(form["grant_type"], "refresh_token");
                    assert_eq!(form["refresh_token"], "first-refresh");
                    assert!(!form.contains_key("scope"));
                }
                let mut reply = serde_json::json!({
                    "access_token": if index == 0 {"first-access"} else {"second-access"},
                    "refresh_token":if index == 0 {"first-refresh"} else {"second-refresh"},
                    "token_type":"Bearer", "expires_in":3600,
                    "scope":if index == 0 {"openid email"} else {crate::PLAN_SCOPE}
                });
                if index == 0 {
                    reply["id_token"] = token.clone().into();
                }
                reply.to_string()
            };
            write!(socket, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).expect("response");
        }
    });
    let network = tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None);
    let client = Client::at_issuer(&network, &issuer).expect("client");
    let executor = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let credential = executor
        .block_on(client.exchange(callback))
        .expect("exchange");
    assert_eq!(credential.access_token(), Err(Error::PlanDisabled));
    let rotated = executor
        .block_on(client.refresh(&credential))
        .expect("refresh");
    assert_eq!(rotated.access_token(), Ok("second-access"));
    assert_eq!(rotated.refresh_token(), Ok("second-refresh"));
    assert_eq!(rotated.subject(), credential.subject());
    assert_eq!(rotated.client_id(), "oaiapp_test");
    peer.join().expect("peer");
}
