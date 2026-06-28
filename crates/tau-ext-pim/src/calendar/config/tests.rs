use super::*;

#[test]
fn google_config_normalizes_api_base_and_rejects_query_fragments() {
    // The backend appends fixed API paths and query strings, so accepting a
    // configured query or fragment would create ambiguous request targets.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            backend: Some(google_backend_with_api_base("https://proxy.example/api///")),
            ..Default::default()
        }],
        ..Default::default()
    };

    let config = cfg.validate().expect("api base validates");
    let account = config.accounts.get("google").expect("google account");
    let Some(ValidatedBackendConfig::Google { api_base, .. }) = &account.backend else {
        panic!("google backend expected");
    };
    assert_eq!(api_base.as_deref(), Some("https://proxy.example/api"));

    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            backend: Some(google_backend_with_api_base(
                "https://proxy.example/api?x=1",
            )),
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = match cfg.validate() {
        Ok(_) => panic!("query must be rejected"),
        Err(err) => err,
    };
    assert!(err.contains("query or fragment"), "{err}");
}

#[test]
fn google_config_rejects_unsafe_secret_names() {
    // Secret names are looked up in the harness-provided map, not shell
    // expanded paths. Keep them narrow to avoid surprising config meanings.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "../client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: Some("refresh".to_owned()),
                api_base: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = match cfg.validate() {
        Ok(_) => panic!("path-like secret must be rejected"),
        Err(err) => err,
    };
    assert!(err.contains("may only contain"), "{err}");
}

#[test]
fn google_config_allows_http_only_for_loopback_api_base() {
    // A non-HTTPS API base receives bearer tokens; keep plain HTTP limited
    // to local test proxies.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            backend: Some(google_backend_with_api_base("http://127.0.0.1:8080/api")),
            ..Default::default()
        }],
        ..Default::default()
    };
    cfg.validate().expect("loopback http validates");

    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            backend: Some(google_backend_with_api_base("http://proxy.example/api")),
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = match cfg.validate() {
        Ok(_) => panic!("non-loopback http must be rejected"),
        Err(err) => err,
    };
    assert!(err.contains("loopback"), "{err}");
}

#[test]
fn google_config_allows_missing_refresh_token_for_action_auth() {
    // Interactive `/calendar auth google` stores refresh tokens in private
    // extension state, so the manual refresh token secret is optional.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: None,
                api_base: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let config = cfg.validate().expect("missing refresh token is allowed");
    let account = config.accounts.get("google").expect("google account");
    let Some(ValidatedBackendConfig::Google {
        refresh_token_secret,
        ..
    }) = &account.backend
    else {
        panic!("google backend expected");
    };
    assert!(refresh_token_secret.is_none());
}

#[test]
fn ics_feed_url_secret_names_are_validated() {
    // Secret names are harness keys, not filesystem paths. Reject path-like
    // names at configuration validation time before runtime lookup.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: Some("../feed-url".to_owned()),
                url: None,
                allow_plain_http: false,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = match cfg.validate() {
        Ok(_) => panic!("path-like secret is rejected"),
        Err(err) => err,
    };
    assert!(err.contains("may only contain"), "{err}");
}

#[test]
fn caldav_backend_is_rejected_until_implemented() {
    // Accepting a backend that runtime read/write paths ignore makes enabled
    // accounts look configured while every useful command fails later.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "dav".to_owned(),
            backend: Some(CalendarBackendConfig::Caldav {
                url: Some("https://calendar.example.test/dav".to_owned()),
                login: Some("alice".to_owned()),
                password_secret: Some("dav_password".to_owned()),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = match cfg.validate() {
        Ok(_) => panic!("unimplemented backend is rejected"),
        Err(err) => err,
    };
    assert!(err.contains("not implemented"), "{err}");
}

fn google_backend_with_api_base(api_base: &str) -> CalendarBackendConfig {
    CalendarBackendConfig::Google {
        client_id_secret: "client".to_owned(),
        client_secret_secret: None,
        refresh_token_secret: Some("refresh".to_owned()),
        api_base: Some(api_base.to_owned()),
    }
}
