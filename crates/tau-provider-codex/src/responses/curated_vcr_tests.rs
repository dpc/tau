use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tau_proto::{AgentId, PromptContext, PromptOriginator, SessionId};

use super::*;

const FIXTURE_DIR: &str = "fixtures/provider-vcr";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
// Fixture format versions stay at zero per
// `GATE-no-backward-compatibility`.
const FIXTURE_FORMAT_VERSION: u32 = 0;

/// Review manifest for the deliberately synthetic public cassette corpus.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    /// Manifest format version.
    version: u32,
    /// Structural sanitizer policy version.
    redaction_version: u32,
    /// Ordered cassette declarations.
    cassettes: Vec<FixtureDeclaration>,
}

/// One public-safe cassette and its declared compatibility outcome.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FixtureDeclaration {
    /// Logical VCR key and filename stem.
    key: String,
    /// Adapter that owns the wire contract.
    provider: String,
    /// Persisted cassette schema name.
    cassette_schema: String,
    /// Persisted cassette schema version.
    cassette_version: u32,
    /// Provider surface under compatibility test.
    surface: String,
    /// Wire transport parsed during replay.
    transport: FixtureTransport,
    /// Provenance classification; public fixtures must be synthetic.
    source: String,
    /// Human-reviewable compatibility fact supplied by this fixture.
    intent: String,
    /// Explicit publication classification.
    public_safe: bool,
    /// Expected typed replay disposition.
    expected: String,
}

/// Provider transport represented by a fixture.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum FixtureTransport {
    /// Responses WebSocket text frames.
    Websocket,
}

/// The curated compatibility lane parses every declared cassette through the
/// production Responses parser, requires a terminal, and audits publication
/// hygiene. It has no live path and fails if the corpus is empty.
#[test]
fn curated_provider_vcr_replay_only_lane() {
    if std::env::var_os("TAU_CURATED_VCR_LANE").is_some() {
        assert_eq!(
            std::env::var("TAU_VCR").as_deref(),
            Ok("replay-only"),
            "the dedicated lane must force replay-only mode"
        );
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    let manifest_path = root.join("manifest.yaml");
    audit_file_size(&manifest_path, MAX_MANIFEST_BYTES);
    let manifest_text = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
    audit_public_text(&manifest_text).expect("public-safe manifest text");
    let manifest: FixtureManifest =
        serde_yaml_ng::from_str(&manifest_text).expect("parse strict fixture manifest");
    assert_eq!(
        serde_yaml_ng::to_string(&manifest).expect("canonical manifest"),
        manifest_text,
        "manifest YAML is not canonical"
    );
    assert_eq!(
        manifest.version, FIXTURE_FORMAT_VERSION,
        "unsupported manifest version"
    );
    assert_eq!(
        manifest.redaction_version, FIXTURE_FORMAT_VERSION,
        "unsupported redaction policy"
    );
    assert!(
        !manifest.cassettes.is_empty(),
        "replay lane must execute at least one cassette"
    );

    let vcr_config = if std::env::var_os("TAU_CURATED_VCR_LANE").is_some() {
        let config = tau_vcr::VcrConfig::from_env().expect("dedicated lane VCR config");
        assert_eq!(config.mode, tau_vcr::VcrMode::ReplayOnly);
        assert_eq!(
            std::fs::canonicalize(&config.dir).expect("configured VCR directory"),
            std::fs::canonicalize(&root).expect("fixture directory"),
        );
        config
    } else {
        tau_vcr::VcrConfig::new(tau_vcr::VcrMode::ReplayOnly, &root)
    };
    let mut keys = HashSet::new();
    let mut replayed = 0_usize;
    for declaration in manifest.cassettes {
        assert!(
            keys.insert(declaration.key.clone()),
            "duplicate cassette key"
        );
        assert_eq!(declaration.surface, "responses");
        assert_eq!(declaration.provider, "chatgpt");
        assert_eq!(declaration.cassette_schema, "provider-stream");
        assert_eq!(
            declaration.cassette_version,
            PROVIDER_STREAM_CASSETTE_VERSION
        );
        assert_eq!(declaration.source, "synthetic");
        assert!(
            !declaration.intent.trim().is_empty(),
            "missing fixture intent"
        );
        assert!(declaration.public_safe, "private cassette in public corpus");
        assert_eq!(declaration.expected, "success");
        let cassette_path = root.join(format!("{}.yaml", declaration.key));
        audit_file_size(&cassette_path, 1024 * 1024);
        let cassette_text =
            std::fs::read_to_string(&cassette_path).expect("read declared cassette");
        audit_public_text(&cassette_text).expect("public-safe cassette text");
        let session_id = SessionId::parse("curated").expect("known-safe SessionId must be valid");
        let agent_id = AgentId::parse("curated-agent").expect("synthetic agent id");
        let context = PromptContext::default();
        let originator = PromptOriginator::User;
        let request = PromptPayload {
            system_prompt: "",
            context: &context,
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            compaction: None,
            originator: &originator,
            session_id: &session_id,
            agent_id: &agent_id,
            share_user_cache_key: false,
            debug_provider_requests: false,
        };
        let request_projection = serde_json::json!({
            "model": "synthetic-model",
            "input_shape": "single-synthetic-text",
        });
        let transport = tau_proto::ProviderBackendTransport::Websocket;
        assert_eq!(
            provider_vcr_key(&request, "success", transport),
            declaration.key
        );
        let cassette = load_provider_stream_cassette_candidates(
            &vcr_config,
            &request,
            "success",
            transport,
            std::slice::from_ref(&request_projection),
        )
        .expect("production replay-only load")
        .expect("replay-only must never fall back to live");
        assert_eq!(
            serde_yaml_ng::to_string(&cassette).expect("canonical cassette"),
            cassette_text,
            "cassette YAML is not canonical"
        );
        assert_sanitized_request_projection(&cassette.request);
        audit_structural_stream(&cassette.stream);
        let state = ws::run_replay(&cassette.stream, &mut |_| {})
            .expect("production parser must accept curated wire evidence");
        assert!(
            state.provider_terminal_event.is_some(),
            "declared success must have a typed provider terminal"
        );
        replayed += 1;
    }
    assert_eq!(
        replayed,
        keys.len(),
        "every declaration must replay exactly once"
    );
    let mut fixture_keys = HashSet::new();
    for entry in std::fs::read_dir(&root).expect("fixture directory") {
        let entry = entry.expect("read fixture directory entry");
        assert!(
            entry.file_type().expect("fixture file type").is_file(),
            "fixture directory contains a non-file"
        );
        let name = entry
            .file_name()
            .into_string()
            .expect("fixture filename must be UTF-8");
        assert!(name.ends_with(".yaml"), "unexpected fixture file `{name}`");
        if name != "manifest.yaml" {
            fixture_keys.insert(name.trim_end_matches(".yaml").to_owned());
        }
    }
    assert_eq!(keys, fixture_keys, "manifest and cassette files must match");
}

fn audit_file_size(path: &Path, limit: u64) {
    let bytes = std::fs::metadata(path).expect("fixture metadata").len();
    assert!(bytes <= limit, "{} exceeds {limit} bytes", path.display());
}

fn audit_public_text(text: &str) -> Result<(), &'static str> {
    let lowercase = text.to_ascii_lowercase();
    for forbidden in [
        "authorization",
        "bearer ",
        "api_key",
        "cookie",
        "account",
        "reasoning",
        "encrypted_content",
        "tool_output",
        "/home/",
        "/users/",
        "c:\\",
        "sk-",
        "akia",
        "ghp_",
        "xoxb-",
    ] {
        if lowercase.contains(forbidden) {
            return Err("public fixture contains a forbidden category");
        }
    }
    if text.contains('@') {
        return Err("public fixture contains an email-like value");
    }
    for token in text.split(|character: char| !character.is_ascii_alphanumeric()) {
        if looks_high_entropy(token) {
            return Err("public fixture contains a suspicious high-entropy token");
        }
    }
    Ok(())
}

fn looks_high_entropy(token: &str) -> bool {
    // Public fixtures reject mixed-class tokens at 24 bytes once they also
    // contain at least 12 distinct symbols. This intentionally catches common
    // compact credentials while allowing long repetitive schema prose.
    if token.len() < 24 {
        return false;
    }
    let has_lower = token.bytes().any(|byte| byte.is_ascii_lowercase());
    let has_upper = token.bytes().any(|byte| byte.is_ascii_uppercase());
    let has_digit = token.bytes().any(|byte| byte.is_ascii_digit());
    let unique = token.bytes().collect::<HashSet<_>>().len();
    12 <= unique && usize::from(has_lower) + usize::from(has_upper) + usize::from(has_digit) >= 2
}

/// Secondary public-text scanning rejects representative credential, identity,
/// email, path, and entropy leaks while retaining ordinary schema prose.
#[test]
fn public_fixture_text_audit_rejects_sensitive_categories() {
    for forbidden in [
        "Authorization: secret",
        "ghp_shortcredential",
        "user@example.test",
        "/home/private/file",
        "Ab3Def5Ghi7Jkl9Mno2Pqr4S",
    ] {
        assert!(audit_public_text(forbidden).is_err());
    }
    audit_public_text(
        "response output text delta synthetic compatibility aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .expect("ordinary schema prose and long low-entropy token");
}

fn audit_structural_stream(stream: &ProviderRawEventStream) {
    for event in &stream.raw_events {
        let json = event
            .raw
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or(&event.raw);
        let value: serde_json::Value = serde_json::from_str(json).expect("fixture JSON frame");
        let object = value.as_object().expect("fixture event object");
        match value["type"].as_str() {
            Some("response.output_text.delta") => {
                assert_eq!(object.len(), 2);
                assert_eq!(value["delta"], "synthetic compatibility output");
            }
            Some("response.completed") => {
                assert_eq!(object.len(), 2);
                let response = value["response"].as_object().expect("terminal response");
                assert_eq!(response.len(), 2);
                assert_eq!(response["status"], "completed");
                let usage = response["usage"].as_object().expect("synthetic usage");
                assert_eq!(usage.len(), 2);
                assert_eq!(usage["input_tokens"], 1);
                assert_eq!(usage["output_tokens"], 2);
            }
            other => panic!("unallowlisted fixture event type: {other:?}"),
        }
    }
}

fn assert_sanitized_request_projection(request: &serde_json::Value) {
    let object = request.as_object().expect("request projection object");
    assert_eq!(object.len(), 2, "request projection has unexpected keys");
    assert_eq!(request["model"], "synthetic-model");
    assert_eq!(request["input_shape"], "single-synthetic-text");
}
