//! Real utility preparation, harness rooting, cold replay and provider fetch.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tau_client::ArtifactUpload;
use tau_proto::{ArtifactRequest, ArtifactRequestId, ToolResultContentPart};

use super::*;

/// Exact wrapper invoking the production utility entrypoint, never PATH.
const UTILS: &str = env!("CARGO_BIN_EXE_tau-e2e-utils");
/// Sole fixed tool call authored by this fixture's bounded provider script.
const CALL: &str = "prepared-image-call";

/// Persistent invocations store references to re-encoded prepared bytes and
/// cold providers fetch them after simulated catalog loss. Ephemeral
/// agents retain the same image inline without creating prepared artifacts or
/// roots.
#[test]
fn provider_builtin_prepared_images_replay_and_ephemeral_fallback()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = provider_builtin_binary()? else {
        eprintln!("skipping image E2E: set TAU_E2E_PROVIDER_BUILTIN_BIN to the candidate binary");
        return Ok(());
    };
    for ephemeral in [false, true] {
        let fixture = ProviderBuiltinFixture::new_images(
            if ephemeral {
                "prepared-image-ephemeral"
            } else {
                "prepared-image-durable"
            },
            &provider,
            UTILS,
        )?;
        fixture.mark_daemon_started();
        let socket = fixture.socket_path();
        let mut daemon = Some(DaemonGuard::spawn(&fixture, &socket)?);
        let mut peer = connect_images(&socket)?;
        let mut observed = Vec::new();
        upload_original(&mut peer, &mut observed)?;
        peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
            UiCreateAgent {
                request_id: "create-image-agent".into(),
                literal: false,
                session_id: session_id(),
                role: "provider-builtin-images".into(),
                model_override: None,
                effort_override: None,
                metadata: Vec::new(),
                initial_prompt: None,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("image-first".into()),
                parent_agent: None,
                ephemeral,
            },
        )))?;
        let created_agent = wait_created(&mut peer, &mut observed)?;
        if !ephemeral {
            wait_prepared_agent(&fixture, &created_agent)?;
        }
        peer.send(&HarnessInputMessage::emit(Event::UiPromptSubmitted(
            UiPromptSubmitted {
                literal: false,
                session_id: session_id(),
                text: "inspect the fixed original image".into(),
                agent_id: created_agent.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("image-first".into()),
            },
        )))?;
        let initial = fixture.recv_request()?;
        assert_eq!(initial.body["model"], "image-model");
        let continuation = fixture.recv_request()?;
        let bytes = image_bytes(&continuation)?;
        assert_ne!(
            bytes,
            tau_ext_test_dummy::TYPED_IMAGE_PNG,
            "prepared metadata-stripped bytes, not the original artifact"
        );
        assert_eq!(
            image::load_from_memory(&bytes)?.to_rgba8(),
            image::load_from_memory(tau_ext_test_dummy::TYPED_IMAGE_PNG)?.to_rgba8(),
        );
        let prepared_digest =
            tau_proto::ArtifactIdentity::metadata(None, Some("image/png".into()))?
                .digest(&bytes)
                .to_hex();
        let prepared_key =
            tau_proto::ArtifactKey::from_digest(prepared_digest.as_str())?.to_string();
        let finished = wait_text(&mut peer, &mut observed, "image accepted")?;
        let agent_id = finished.agent_id;
        assert_eq!(agent_id, created_agent);
        let expected_policy = if ephemeral {
            tau_proto::PreparedImageStorage::Inline
        } else {
            tau_proto::PreparedImageStorage::Artifact
        };
        assert!(observed.iter().any(|event| matches!(event,
            Event::ToolStarted(started) if started.call_id.as_str() == CALL
                && started.invocation_policy.prepared_image_storage == expected_policy)));
        require_private_projection(&observed, &prepared_key, &bytes)?;

        let snapshot = if ephemeral {
            assert!(
                !fixture
                    .harness_state_dir()
                    .join("artifacts/transcript-roots")
                    .join(agent_id.as_str())
                    .exists()
            );
            assert!(
                !fixture
                    .harness_state_dir()
                    .join("artifacts/blake3")
                    .join(prepared_digest.as_str())
                    .exists()
            );
            None
        } else {
            disconnect_ui(&mut peer)?;
            daemon.take().expect("first daemon").finish()?;
            let snapshot = DurableSnapshot::load(fixture.harness_state_dir(), &session_id())?;
            let result = snapshot
                .agent_events
                .iter()
                .find_map(|record| match &record.event {
                    Event::ProviderToolResult(result) if result.call_id.as_str() == CALL => {
                        Some(result)
                    }
                    _ => None,
                })
                .ok_or("canonical image result missing")?;
            let [ToolResultContentPart::ImageReference(reference)] =
                result.provider_content.as_slice()
            else {
                return Err("persistent image was not stored as a reference".into());
            };
            assert_eq!(reference.blob.key.as_str(), prepared_key);
            assert_eq!(reference.blob.size.get(), bytes.len() as u64);
            let digest = prepared_digest.as_str();
            let artifacts = fixture.harness_state_dir().join("artifacts");
            let roots = std::fs::read_to_string(
                artifacts
                    .join("transcript-roots")
                    .join(agent_id.as_str())
                    .join("roots.json"),
            )?;
            assert!(roots.contains(&prepared_key));
            assert_eq!(
                std::fs::read(
                    artifacts
                        .join("transcript-objects")
                        .join(digest)
                        .join("data")
                )?,
                bytes
            );
            let canonical = artifacts.join("blake3").join(digest);
            let metadata: serde_json::Value =
                serde_json::from_slice(&std::fs::read(canonical.join("meta.json"))?)?;
            assert_eq!(
                metadata["version"], 2,
                "metadata-bound identity uses its explicit storage schema"
            );
            // Simulate ordinary catalog loss after clean daemon shutdown.
            // Legacy cleaners preserve the new metadata version; the alias
            // still supports recovery independently of the ordinary catalog.
            std::fs::remove_dir_all(canonical)?;
            let resumed = DaemonGuard::resume(&fixture, &socket)?;
            peer = connect_images(&socket)?;
            wait_replay(&mut peer, &mut observed)?;
            submit_p2(&mut peer, &agent_id)?;
            let replayed = fixture.recv_request()?;
            assert_eq!(image_bytes(&replayed)?, bytes);
            wait_text(&mut peer, &mut observed, "image replay accepted")?;
            require_private_projection(&observed, &prepared_key, &bytes)?;
            disconnect_ui(&mut peer)?;
            resumed.finish()?;
            let cold = DurableSnapshot::load(fixture.harness_state_dir(), &session_id())?;
            cold.require_prefix(&snapshot)?;
            assert_eq!(
                cold.agent_events
                    .iter()
                    .filter(|record| matches!(&record.event,
                Event::ProviderToolResult(result) if result.call_id.as_str() == CALL))
                    .count(),
                1
            );
            Some(snapshot)
        };
        if snapshot.is_none() {
            submit_p2(&mut peer, &agent_id)?;
            assert_eq!(image_bytes(&fixture.recv_request()?)?, bytes);
            wait_text(&mut peer, &mut observed, "image replay accepted")?;
            require_private_projection(&observed, &prepared_key, &bytes)?;
            disconnect_ui(&mut peer)?;
            daemon.take().expect("ephemeral daemon").finish()?;
        }
        fixture.finish()?;
    }
    Ok(())
}

/// Reads the ordinary create acknowledgment without starting provider work.
fn wait_created(
    peer: &mut SocketPeer,
    observed: &mut Vec<Event>,
) -> Result<tau_proto::AgentId, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + EVENT_WATCHDOG;
    loop {
        if let Event::UiCreateAgentResult(created) = next_event(peer, observed, deadline)?
            && created.request_id == "create-image-agent"
        {
            return match created.outcome {
                tau_proto::UiCreateAgentOutcome::Created { agent_id, .. } => Ok(agent_id),
                _ => Err("image fixture agent creation rejected".into()),
            };
        }
    }
}

/// A nonempty private journal proves the creation worker has already installed
/// Prepared authority: core sets that lifecycle before its first append. This
/// is not a claim that file size proves fsync durability. Watch-before-check
/// ordering avoids racing creation and never polls or changes production flow.
fn wait_prepared_agent(
    fixture: &ProviderBuiltinFixture,
    agent_id: &tau_proto::AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;

    use nix::poll::{PollFd, PollFlags, poll};
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    let root = fixture.harness_state_dir().join("agents");
    let agent = root.join(agent_id.as_str());
    let journal = agent.join("events.cbor");
    let watch = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)?;
    let mask = AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MODIFY | AddWatchFlags::IN_MOVED_TO;
    watch.add_watch(root.as_path(), mask)?;
    let deadline = Instant::now() + EVENT_WATCHDOG;
    let mut watching_agent = false;
    loop {
        if !watching_agent && agent.exists() {
            watch.add_watch(agent.as_path(), mask)?;
            watching_agent = true;
        }
        if std::fs::metadata(&journal)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() != 0)
        {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("agent creation watchdog expired".into());
        }
        let timeout = u16::try_from(remaining.as_millis().max(1))?;
        let mut descriptors = [PollFd::new(watch.as_fd(), PollFlags::POLLIN)];
        if poll(&mut descriptors, timeout)? == 0 {
            return Err("agent creation watchdog expired".into());
        }
        watch.read_events()?;
    }
}

/// Observes ordinary generic events, including notices and replay; directed
/// Artifact replies are kept separate from this disclosure oracle.
fn connect_images(socket: &Path) -> Result<SocketPeer, Box<dyn std::error::Error>> {
    let mut peer = connect_peer_until(
        socket,
        Instant::now() + EVENT_WATCHDOG,
        |path| SocketPeer::connect(path),
        Instant::now,
        thread::yield_now,
    )?;
    peer.send(&HarnessInputMessage::Hello(Hello {
        declaration_inspection: false,
        build_identity: None,
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: "prepared-image-observer".parse()?,
        client_kind: ClientKind::Ui,
        expected_session_id: Some(session_id()),
        capabilities: Default::default(),
    }))?;
    let selectors = [
        "agent.",
        "provider.",
        "tool.",
        "harness.",
        "session.",
        "ui.",
    ]
    .into_iter()
    .map(|prefix| EventSelector::Prefix(prefix.into()))
    .collect::<Vec<_>>();
    peer.send(&HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: selectors.clone(),
        live_selectors: selectors,
    }))?;
    Ok(peer)
}

/// Publishes only the compiled original through the public UI Artifact channel.
fn upload_original(
    peer: &mut SocketPeer,
    observed: &mut Vec<Event>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut upload = ArtifactUpload::new(tau_ext_test_dummy::TYPED_IMAGE_PNG.to_vec())?;
    let mut ordinal = 0;
    while let Some(op) = upload.next_op() {
        ordinal += 1;
        let id: ArtifactRequestId = format!("image-source-{ordinal}").parse()?;
        peer.send(&HarnessInputMessage::ArtifactRequest(ArtifactRequest {
            request_id: id.clone(),
            expected_session_id: session_id(),
            op,
        }))?;
        let deadline = Instant::now() + EVENT_WATCHDOG;
        loop {
            match peer.recv_timeout(deadline.saturating_duration_since(Instant::now()))? {
                SocketReceive::Message {
                    message: HarnessOutputMessage::ArtifactResult(result),
                } if result.request_id == id => {
                    upload.accept(result.result?)?;
                    break;
                }
                SocketReceive::Message {
                    message: HarnessOutputMessage::Deliver(delivery),
                } => observed.push(delivery.into_parts().0),
                SocketReceive::Message { .. } => {}
                _ => return Err("source upload did not finish".into()),
            }
        }
    }
    assert_eq!(
        upload.descriptor().ok_or("descriptor")?.key.as_str(),
        "c8bc6ccc266fb3d80c7fe1bb"
    );
    Ok(())
}

/// Extracts the sole upstream data URL, never trusting canonical descriptor
/// text as evidence that the provider actually consumed the prepared bytes.
fn image_bytes(request: &CapturedChatRequest) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let urls: Vec<_> = request.body["messages"]
        .as_array()
        .ok_or("messages")?
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter_map(|part| part["image_url"]["url"].as_str())
        .collect();
    let [url] = urls.as_slice() else {
        return Err("one upstream image required".into());
    };
    Ok(STANDARD.decode(
        url.strip_prefix("data:image/png;base64,")
            .ok_or("PNG data URL")?,
    )?)
}

/// Accumulates generic events while preserving a single absolute outer
/// watchdog.
fn next_event(
    peer: &mut SocketPeer,
    observed: &mut Vec<Event>,
    deadline: Instant,
) -> Result<Event, Box<dyn std::error::Error>> {
    loop {
        match peer.recv_timeout(deadline.saturating_duration_since(Instant::now()))? {
            SocketReceive::Message {
                message: HarnessOutputMessage::Deliver(delivery),
            } => {
                let event = delivery.into_parts().0;
                observed.push(event.clone());
                return Ok(event);
            }
            SocketReceive::Message {
                message: HarnessOutputMessage::Disconnect(_),
            } => return Err("unexpected daemon disconnect".into()),
            SocketReceive::Message { .. } => {}
            _ => return Err("image lifecycle watchdog expired".into()),
        }
    }
}

/// Waits for visible success rather than sleeping before shutdown.
fn wait_text(
    peer: &mut SocketPeer,
    observed: &mut Vec<Event>,
    text: &str,
) -> Result<tau_proto::ProviderResponseFinished, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + EVENT_WATCHDOG;
    loop {
        if let Event::ProviderResponseFinished(finished) = next_event(peer, observed, deadline)?
            && finished.output_items.iter().any(|item| matches!(item,
                tau_proto::ContextItem::Message(message) if message.content.iter().any(|part|
                    matches!(part, tau_proto::ContentPart::Text { text: actual } if actual == text))))
        {
            return Ok(finished);
        }
    }
}

/// Cold requests begin only after the ordinary replay handoff completes.
fn wait_replay(
    peer: &mut SocketPeer,
    observed: &mut Vec<Event>,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + EVENT_WATCHDOG;
    loop {
        if matches!(
            next_event(peer, observed, deadline)?,
            Event::SessionReplayComplete(_)
        ) {
            return Ok(());
        }
    }
}

/// Generic notices, live events and cold replay must expose neither fetchable
/// prepared keys nor the corresponding encoded bytes.
fn require_private_projection(
    events: &[Event],
    key: &str,
    bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string(events)?;
    assert!(
        !json.contains(key),
        "prepared key escaped generic projection"
    );
    assert!(
        !json.contains(&STANDARD.encode(bytes)),
        "prepared bytes escaped generic projection"
    );
    assert!(
        !json.contains(&serde_json::to_string(bytes)?),
        "prepared byte array escaped generic projection"
    );
    Ok(())
}
