#!/usr/bin/env bash
set -euo pipefail

workspace=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

registry_consumer=false
if [[ "${1:-}" == "--registry" ]]; then
  registry_consumer=true
  shift
fi
if (($# != 0)); then
  echo "usage: $0 [--registry]" >&2
  exit 2
fi

packages=(
  dpc-tau-actions
  dpc-tau-blocking-notify-channel
  dpc-tau-proto
  dpc-tau-client
)
msrv_nixpkgs=github:NixOS/nixpkgs/b6018f87da91d19d0ab4cf979885689b469cdd41

export CARGO_TARGET_DIR="$tmp/target"
export CARGO_TERM_COLOR=never
export CARGO_TERM_PROGRESS_WHEN=never

cd "$workspace"
metadata=$(cargo metadata --format-version 1 --no-deps)
declare -A versions
for package in "${packages[@]}"; do
  versions["$package"]=$(
    python3 -c '
import json
import sys

package = sys.argv[1]
metadata = json.load(sys.stdin)
matches = [item["version"] for item in metadata["packages"] if item["name"] == package]
if len(matches) != 1:
    raise SystemExit(f"expected one metadata entry for {package}, found {len(matches)}")
print(matches[0])
' "$package" <<<"$metadata"
  )
done

cargo package \
  --locked \
  --allow-dirty \
  --no-verify \
  "${packages[@]/#/-p}"

mkdir "$tmp/packages"
for package in "${packages[@]}"; do
  version=${versions["$package"]}
  archive="$CARGO_TARGET_DIR/package/${package}-${version}.crate"
  test -f "$archive"
  tar -xzf "$archive" -C "$tmp/packages"

  manifest="$tmp/packages/${package}-${version}/Cargo.toml"
  python3 - "$package" "$manifest" <<'PY'
import pathlib
import sys
import tomllib

package = sys.argv[1]
manifest = tomllib.loads(pathlib.Path(sys.argv[2]).read_text())

expected_internal_dependencies = {
    "dpc-tau-actions": {},
    "dpc-tau-blocking-notify-channel": {},
    "dpc-tau-proto": {
        "dpc-tau-actions": "=0.1.0",
    },
    "dpc-tau-client": {
        "dpc-tau-blocking-notify-channel": "=0.1.0",
        "dpc-tau-proto": "=0.16.0",
    },
}

def check_dependencies(table):
    for dependency, value in table.items():
        if isinstance(value, dict) and "path" in value:
            raise SystemExit(
                f"{package} package retains path dependency {dependency}"
            )

expected = expected_internal_dependencies[package]
actual_internal_dependency_names = set()

def check_internal_dependencies(table):
    for dependency, value in table.items():
        package_name = value.get("package", dependency) if isinstance(value, dict) else dependency
        if package_name.startswith("dpc-tau-"):
            version = value.get("version") if isinstance(value, dict) else value
            actual_internal_dependency_names.add(package_name)
            expected_version = expected.get(package_name)
            if version != expected_version:
                raise SystemExit(
                    f"{package} dependency {package_name} is {version!r}, "
                    f"expected exact {expected_version!r}"
                )

if manifest.get("workspace") is not None:
    raise SystemExit(f"{package} package retains workspace inheritance")

for key in ("dependencies", "dev-dependencies", "build-dependencies"):
    table = manifest.get(key, {})
    check_dependencies(table)
    check_internal_dependencies(table)
for target in manifest.get("target", {}).values():
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = target.get(key, {})
        check_dependencies(table)
        check_internal_dependencies(table)

if actual_internal_dependency_names != set(expected):
    raise SystemExit(
        f"{package} internal dependency names are "
        f"{sorted(actual_internal_dependency_names)!r}, "
        f"expected exact closure {sorted(expected)!r}"
    )
PY
  test -f "$tmp/packages/${package}-${version}/README.md"
done

mkdir -p "$tmp/consumer/src"
cat >"$tmp/consumer/Cargo.toml" <<EOF
[package]
name = "tau-sdk-package-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.91"
publish = false

[dependencies]
tau-actions = { package = "dpc-tau-actions", version = "=${versions[dpc-tau-actions]}" }
tau-blocking-notify-channel = { package = "dpc-tau-blocking-notify-channel", version = "=${versions[dpc-tau-blocking-notify-channel]}" }
tau-client = { package = "dpc-tau-client", version = "=${versions[dpc-tau-client]}" }
tau-proto = { package = "dpc-tau-proto", version = "=${versions[dpc-tau-proto]}" }
EOF

if [[ "$registry_consumer" == false ]]; then
  cat >>"$tmp/consumer/Cargo.toml" <<EOF

[patch.crates-io]
dpc-tau-actions = { path = "$tmp/packages/dpc-tau-actions-${versions[dpc-tau-actions]}" }
dpc-tau-blocking-notify-channel = { path = "$tmp/packages/dpc-tau-blocking-notify-channel-${versions[dpc-tau-blocking-notify-channel]}" }
dpc-tau-client = { path = "$tmp/packages/dpc-tau-client-${versions[dpc-tau-client]}" }
dpc-tau-proto = { path = "$tmp/packages/dpc-tau-proto-${versions[dpc-tau-proto]}" }
EOF
fi

cat >"$tmp/consumer/src/lib.rs" <<'EOF'
/// Verifies that the packaged protocol and client surfaces resolve together.
#[test]
fn packaged_sdk_round_trips_the_advertised_protocol_version() {
    let encoded = tau_proto::encode_message_to_vec(&tau_proto::PROTOCOL_VERSION)
        .expect("protocol version should encode");
    let decoded: tau_proto::ProtocolVersion =
        tau_proto::decode_message_from_slice(&encoded).expect("protocol version should decode");

    assert_eq!(decoded, tau_proto::ProtocolVersion::new(16, 0));
    let _logging_initializer: fn(&'static str) = tau_client::init_logging_for;
}

/// Consumers must distinguish review acceptance from worker delivery and retain
/// the durable review reference when main releases the original message.
#[test]
fn packaged_sdk_preserves_peer_review_routing() {
    use tau_proto::*;
    let main: AgentId = "main".parse().expect("main");
    let worker: AgentId = "worker".parse().expect("worker");
    let ack = ExternalAgentMessageResult {
        request_id: "request".into(),
        failure: None,
        recipient_id: Some(main.clone()),
        review_recipient: Some(worker.clone()),
        started: false,
    };
    let bytes = encode_message_to_vec(&ack).expect("encode ack");
    let decoded: ExternalAgentMessageResult = decode_message_from_slice(&bytes).expect("decode ack");
    assert_eq!(decoded.recipient_id, Some(main.clone()));
    assert_eq!(decoded.review_recipient, Some(worker.clone()));
    assert_ne!(decoded.recipient_id, decoded.review_recipient);
    let sent = Event::AgentMessageSent(AgentMessageSent {
        message_id: "message".parse().expect("message"),
        sender_id: "sender".parse().expect("sender"),
        recipient: AgentMessageRecipient::ExternalAgent {
            session_id: "session".parse().expect("session"),
            agent_id: main.clone(),
        },
        kind: AgentMessageKind::default(),
        sender_notice: None,
        review_recipient: Some(worker.clone()),
        message: "review me".into(),
    });
    let bytes = encode_message_to_vec(&sent).expect("encode sent");
    let decoded: Event = decode_message_from_slice(&bytes).expect("decode sent");
    assert_eq!(decoded, sent);

    for review in [
        PeerMessageReview::Pending { intended_recipient: worker.clone() },
        PeerMessageReview::Released {
            source: PeerMessageReviewReference { main_agent_id: main.clone(), durable_event_seq: 42 },
        },
    ] {
        let recipient_id = match &review {
            PeerMessageReview::Pending { .. } => main.clone(),
            PeerMessageReview::Released { .. } => worker.clone(),
        };
        let received = Event::AgentMessageReceived(AgentMessageReceived {
            message_id: "message".parse().expect("message"),
            sender_id: "sender".parse().expect("sender"),
            sender_session_id: Some("sender-session".parse().expect("sender session")),
            cli_origin: None,
            recipient_id,
            kind: AgentMessageKind::default(),
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            sender_notice: None,
            recipient_notice: None,
            peer_review: Some(review),
            message: "review me".into(),
        });
        let bytes = encode_message_to_vec(&received).expect("encode receive");
        let decoded: Event = decode_message_from_slice(&bytes).expect("decode receive");
        assert_eq!(decoded, received);
    }
}

/// Main ownership survives runtime unavailability; SDK consumers need the
/// distinct event, replay identity, UI request and protected-unload outcome.
#[test]
fn packaged_sdk_preserves_main_agent_lifecycle() {
    use tau_proto::*;
    let session_id: SessionId = "session".parse().expect("session");
    let agent_id: AgentId = "main".parse().expect("main");
    for event in [
        Event::SessionAgentLoaded(SessionAgentLoaded {
            main: true,
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            agent_initialization_id: "initialization".parse().expect("initialization"),
            ephemeral: false,
        }),
        Event::AgentRuntimeUnavailable(AgentRuntimeUnavailable {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
        }),
        Event::SessionReplayComplete(SessionReplayComplete {
            main_agent_id: Some(agent_id),
            session_id,
            error: None,
        }),
    ] {
        let bytes = encode_message_to_vec(&event).expect("encode main event");
        let decoded: Event = decode_message_from_slice(&bytes).expect("decode main event");
        assert_eq!(decoded, event);
    }
    let bytes = encode_message_to_vec(&HarnessInputMessage::UiEnsureMainAgent).expect("encode ensure");
    let decoded: HarnessInputMessage = decode_message_from_slice(&bytes).expect("decode ensure");
    assert!(matches!(decoded, HarnessInputMessage::UiEnsureMainAgent));
    let bytes = encode_message_to_vec(&UnloadSessionAgentOutcome::MainAgent).expect("encode outcome");
    let decoded: UnloadSessionAgentOutcome = decode_message_from_slice(&bytes).expect("decode outcome");
    assert_eq!(decoded, UnloadSessionAgentOutcome::MainAgent);
}

/// Consumers decode stable partial answer history and nonterminal response
/// ownership instead of treating every completed response as a finished turn.
#[test]
fn packaged_sdk_round_trips_explicit_response_continuation() {
    let phase = tau_proto::MessagePhase::PartialAnswer;
    let stop = tau_proto::ProviderStopReason::Continue;
    let bytes = tau_proto::encode_message_to_vec(&(phase, stop)).expect("encode");
    let decoded: (tau_proto::MessagePhase, tau_proto::ProviderStopReason) =
        tau_proto::decode_message_from_slice(&bytes).expect("decode");
    assert_eq!(decoded, (phase, stop));
    assert_eq!(phase.as_openai_wire(), "partial_answer");
    assert!(!stop.requests_tool_calls());
}

/// New consumers distinguish prepared references and default to inline output.
#[test]
fn packaged_sdk_round_trips_prepared_image_reference_and_invocation_policy() {
    let content = tau_proto::ToolResultContentPart::ImageReference(tau_proto::ImageReference {
        blob: tau_proto::ArtifactDescriptor::new(
            tau_proto::ArtifactKey::parse(format!("blake3:{}", "ab".repeat(32))).expect("key"),
            3,
        ).expect("descriptor"),
        media_type: tau_proto::ImageMediaType::Png,
        width: 1,
        height: 1,
        detail: tau_proto::ImageDetail::High,
    });
    let encoded = tau_proto::encode_message_to_vec(&content).expect("encode reference");
    let decoded: tau_proto::ToolResultContentPart =
        tau_proto::decode_message_from_slice(&encoded).expect("decode reference");
    assert_eq!(decoded, content);
    assert!(decoded.is_image_reference());
    assert_eq!(decoded.encoded_image_bytes(), 3);
    let policy = tau_proto::ToolInvocationPolicy::default();
    assert_eq!(policy.prepared_image_storage, tau_proto::PreparedImageStorage::Inline);
}

/// UI discovery DTOs and consumer-local MIME inference must survive packaging.
#[test]
fn packaged_sdk_exposes_clipboard_artifact_helpers() {
    let op = tau_proto::ArtifactOp::List { prefix: String::new(), limit: 10 };
    let encoded = tau_proto::encode_message_to_vec(&op).expect("list encoding");
    let decoded: tau_proto::ArtifactOp =
        tau_proto::decode_message_from_slice(&encoded).expect("list decoding");
    assert_eq!(op, decoded);
    let media = tau_client::ArtifactMedia::select(b"\0binary", None).expect("media");
    assert_eq!(media.mime, "application/octet-stream");
    assert!(!media.plain_alias);
}

/// The source-only SDK bump must include generic publisher and passive append
/// surfaces, not just advertise the new revision.
#[test]
fn packaged_sdk_exposes_generic_passive_context() {
    let request = tau_proto::Event::ExtPassiveContextAppendRequest(
        tau_proto::ExtPassiveContextAppendRequest {
            session_id: "session".parse().expect("session"),
            agent_id: "agent".parse().expect("agent"),
            operation_id: "operation".to_owned(),
            body: "output".to_owned(),
        },
    );
    let encoded = tau_proto::encode_message_to_vec(&request).expect("request encodes");
    let decoded: tau_proto::Event =
        tau_proto::decode_message_from_slice(&encoded).expect("request decodes");
    assert_eq!(request, decoded);
    let mut delivery = tau_proto::EventDelivery::live(tau_proto::UnixMicros::new(1), request);
    delivery.publisher = Some(tau_proto::EventPublisher::Harness);
    let encoded = tau_proto::encode_message_to_vec(&delivery).expect("delivery encodes");
    let decoded: tau_proto::EventDelivery =
        tau_proto::decode_message_from_slice(&encoded).expect("delivery decodes");
    assert_eq!(decoded.publisher, delivery.publisher);
}

/// Prevents version-only SDK updates from omitting compaction recovery authority;
/// both provider and canonical standalone failure DTOs must exist.
#[test]
fn packaged_sdk_preserves_compaction_failure_variants() {
    for provider in [
        tau_proto::ProviderFailureKind::TransientCompactionExhausted,
        tau_proto::ProviderFailureKind::LocalSummaryUnusable,
    ] {
        let encoded = tau_proto::encode_message_to_vec(&provider).expect("provider failure should encode");
        let decoded: tau_proto::ProviderFailureKind =
            tau_proto::decode_message_from_slice(&encoded).expect("provider failure should decode");
        assert_eq!(decoded, provider);
    }
    for standalone in [
        tau_proto::StandaloneCompactionFailureReason::TransientCompactionExhausted,
        tau_proto::StandaloneCompactionFailureReason::LocalSummaryUnusable,
    ] {
        let encoded =
            tau_proto::encode_message_to_vec(&standalone).expect("standalone failure should encode");
        let decoded: tau_proto::StandaloneCompactionFailureReason =
            tau_proto::decode_message_from_slice(&encoded).expect("standalone failure should decode");
        assert_eq!(decoded, standalone);
    }
}

/// Ensures the archive includes protocol 11.1's actual source API and wire field,
/// rather than just advertising that revision over the older discovery DTO.
#[test]
fn packaged_sdk_preserves_original_discovery_paths() {
    let file = tau_proto::DiscoveryAgentsFile {
        file_path: "/resolved/AGENTS.md".into(),
        display_path: Some("/original/AGENTS.md".into()),
        content: "instructions".into(),
    };
    let encoded = tau_proto::encode_message_to_vec(&file).expect("discovery file should encode");
    let decoded: tau_proto::DiscoveryAgentsFile =
        tau_proto::decode_message_from_slice(&encoded).expect("discovery file should decode");
    assert_eq!(decoded, file);
}
EOF

nix shell \
  "$msrv_nixpkgs#cargo" \
  "$msrv_nixpkgs#rustc" \
  --command bash -euo pipefail -c '
    [[ "$(rustc --version)" == "rustc 1.91."* ]]
    [[ "$(cargo --version)" == "cargo 1.91."* ]]
    cd "$1"
    cargo test --manifest-path Cargo.toml -- --color never
  ' bash "$tmp/consumer"
