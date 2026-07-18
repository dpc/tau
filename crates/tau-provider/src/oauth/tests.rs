use std::process::Command;

const CHILD_CASE_ENV: &str = "TAU_PROVIDER_PROXY_TEST_CASE";
const CHILD_TEST_NAME: &str = "oauth::tests::proxy_from_env_child";

#[test]
fn proxy_from_env_honors_no_proxy() {
    run_child_case(
        "honors_no_proxy",
        &[
            ("HTTPS_PROXY", "http://proxy.example:8080"),
            ("NO_PROXY", "api.openai.com"),
        ],
    );
}

#[test]
fn proxy_from_env_accepts_lowercase_no_proxy() {
    run_child_case(
        "accepts_lowercase_no_proxy",
        &[
            ("https_proxy", "http://lower-proxy.example:3128"),
            ("no_proxy", ".internal.example"),
        ],
    );
}

#[test]
fn proxy_from_env_child() {
    let Ok(case) = std::env::var(CHILD_CASE_ENV) else {
        return;
    };

    match case.as_str() {
        "honors_no_proxy" => {
            let proxy = super::proxy_from_env().expect("proxy from env");
            assert_eq!(proxy.host(), "proxy.example");
            assert_eq!(proxy.port(), 8080);
            assert!(proxy.is_from_env());
            assert!(proxy.is_no_proxy(&uri("https://api.openai.com/v1/responses")));
            assert!(!proxy.is_no_proxy(&uri("https://example.com/v1/responses")));
        }
        "accepts_lowercase_no_proxy" => {
            let proxy = super::proxy_from_env().expect("proxy from env");
            assert_eq!(proxy.host(), "lower-proxy.example");
            assert!(proxy.is_no_proxy(&uri("https://service.internal.example/v1")));
            assert!(!proxy.is_no_proxy(&uri("https://service.external.example/v1")));
        }
        other => panic!("unknown proxy child test case: {other}"),
    }
}

fn run_child_case(case: &str, proxy_env: &[(&str, &str)]) {
    let mut command = Command::new(std::env::current_exe().expect("current test binary"));
    command
        .arg("--exact")
        .arg(CHILD_TEST_NAME)
        .arg("--nocapture")
        .env_clear()
        .env(CHILD_CASE_ENV, case);

    for (key, value) in proxy_env {
        command.env(key, value);
    }

    let output = command.output().expect("run proxy child test");
    if !output.status.success() {
        panic!(
            "proxy child test failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn uri(value: &str) -> ureq::http::Uri {
    value.parse().expect("valid URI")
}
