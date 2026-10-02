//! Real process locks with synthetic paths; no credentials or network access.

use std::io::Cursor;
use std::os::unix::fs::symlink;
use std::process::{Child, Command, Stdio};

use super::*;
use crate::tests::SharedTraceWriter;

/// Always reap a fixture child, including assertion unwinds.
struct ChildGuard {
    /// Test binary retaining the advisory lock.
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Child-only lock holder; normal test-suite execution does nothing.
#[test]
fn lock_child() {
    let Some(root) = std::env::var_os("TAU_CHATGPT_LOCK_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let _guard = lock(
        &root,
        &tau_proto::ExtensionDataPath::new("providers/fixture/chatgpt-plan.json"),
        &mut || false,
    )
    .expect("child lock");
    std::fs::write(root.join("ready"), []).expect("signal readiness");
    std::thread::sleep(Duration::from_secs(60));
}

/// Two processes sharing a Secret identity cannot refresh simultaneously, and
/// process death releases ownership without a durable quarantine marker.
#[test]
fn cross_process_lock_contention_and_crash_release() {
    let directory = tempfile::tempdir().expect("fixture root");
    let mut child = ChildGuard {
        child: Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "chatgpt_plan::runtime::tests::lock_child",
                "--nocapture",
            ])
            .env("TAU_CHATGPT_LOCK_TEST_ROOT", directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn lock holder"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !directory.path().join("ready").exists() {
        assert!(Instant::now() < deadline, "child did not acquire lock");
        assert!(child.child.try_wait().expect("child status").is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let path = tau_proto::ExtensionDataPath::new("providers/fixture/chatgpt-plan.json");
    let mut checks = 0;
    assert!(
        lock(directory.path(), &path, &mut || {
            checks += 1;
            checks > 2
        })
        .is_err()
    );
    assert!(checks > 2, "contender never tried the held lock");
    // Another credential in the same instance remains independent.
    let sibling = lock(
        directory.path(),
        &tau_proto::ExtensionDataPath::new("providers/other/chatgpt-plan.json"),
        &mut || false,
    )
    .expect("independent credential");
    drop(sibling);
    child.child.kill().expect("crash holder");
    child.child.wait().expect("reap holder");
    let _recovered = lock(directory.path(), &path, &mut || false).expect("crash-released lock");
}

/// A path substitution cannot turn a credential lock into an unrelated file.
#[test]
fn lock_rejects_symlink_and_cancellation() {
    let directory = tempfile::tempdir().expect("fixture root");
    let path = tau_proto::ExtensionDataPath::new("providers/fixture/chatgpt-plan.json");
    assert!(lock(directory.path(), &path, &mut || true).is_err());
    let key = blake3::hash(path.as_str().as_bytes()).to_hex();
    let lock_path = directory.path().join(format!("chatgpt-plan-{key}.lock"));
    std::fs::remove_file(&lock_path).expect("remove fixture lock");
    symlink(directory.path().join("target"), &lock_path).expect("symlink");
    assert!(lock(directory.path(), &path, &mut || false).is_err());
    assert!(!directory.path().join("target").exists());
}

/// Synthetic protected records exercise production storage logic without OAuth.
fn credential(subject: &str, token: &str, expires_at_ms: u64) -> Credential {
    Credential::decode(
        &serde_json::to_vec(&serde_json::json!({
            "version":0, "client_id":"oaiapp_fixture", "subject":subject,
            "email":null, "scopes":[tau_provider_chatgpt::PLAN_SCOPE],
            "access_token":token, "refresh_token":format!("refresh-{token}"),
            "id_token":null, "expires_at_ms":expires_at_ms
        }))
        .expect("fixture JSON"),
    )
    .expect("fixture credential")
}

/// Minimal SDK runtime supplies the same non-Send main-loop authority and waker
/// as production; only the token endpoint exchange is replaced.
struct TestExtension;

impl tau_client::TauExtension for TestExtension {
    type State =
        crate::ProviderRuntime<fn(Option<&crate::ProviderName>) -> crate::BuiltinProviderProfiles>;

    fn name(&self) -> &'static str {
        "chatgpt-renewal-test"
    }

    fn register(self, _: &mut tau_client::ExtensionBuilder<Self::State>) {}
}

/// Authoritative reads reject account changes, adopt newer generations, publish
/// rotation despite cancellation, and never resurrect a removed credential.
#[test]
fn renewal_uses_authoritative_storage_and_fences_races() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tau_client::TauExtensionRunner;
    use tau_proto::{Configure, ExtensionDataResult, HarnessOutputMessage, HarnessOutputWriter};

    for scenario in 0..6 {
        let mut input = Vec::new();
        let mut writer = HarnessOutputWriter::new(&mut input);
        writer
            .write_message(&HarnessOutputMessage::Configure(Configure {
                purpose: Default::default(),
                harness_protocol_version: None,
                config: tau_proto::CborValue::Map(Vec::new()),
                instance_name: "chatgpt-renewal-test".parse().expect("instance"),
                tool_prefix: None,
                state_dir: None,
                secrets: Default::default(),
                absent_optional_secrets: Default::default(),
                settings_files: Default::default(),
            }))
            .expect("configure");
        writer.flush().expect("flush");
        let mut runtime = TauExtensionRunner::new(TestExtension)
            .start_manual_loop_with_extension_data_state(
                Cursor::new(input),
                SharedTraceWriter::default(),
                |_, client| {
                    let mut state = crate::tests::observation_test_runtime();
                    state.extension_data_client = Some(client);
                    state
                },
            )
            .expect("runtime");
        let directory = tempfile::tempdir().expect("lock root");
        let session = Session {
            root: directory.path().to_owned(),
            path: tau_proto::ExtensionDataPath::new("providers/fixture/chatgpt-plan.json"),
            tx: runtime.state().worker_tx.clone(),
            waker: runtime.waker(),
            failed: Arc::clone(&runtime.state().credential_admission.chatgpt_plan.failed),
        };
        let selected = credential("selected", "stale", 0);
        let future = crate::now_ms() + 3_600_000;
        let current = credential(
            if scenario == 0 { "changed" } else { "selected" },
            "authoritative",
            if scenario == 1 { future } else { 0 },
        );
        let mut stored = current.encode().expect("stored bytes");
        let initial = stored.clone();
        let canceled = Arc::new(AtomicBool::new(false));
        let exchanges = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&exchanges);
        let cancel = Arc::clone(&canceled);
        let worker = std::thread::spawn(move || {
            let result = session.credential_with_exchange(
                &selected,
                &mut || cancel.load(Ordering::SeqCst),
                |current| {
                    count.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(current.access_token().expect("bearer"), "authoritative");
                    if scenario == 4 {
                        return Err(tau_provider_chatgpt::Error::Transport);
                    }
                    if scenario == 5 {
                        return Err(tau_provider_chatgpt::Error::InvalidGrant);
                    }
                    // Publication is required after a successful rotation even
                    // if the original prompt no longer wants inference.
                    cancel.store(true, Ordering::SeqCst);
                    Ok(credential("selected", "rotated", future))
                },
            );
            if scenario == 4 {
                assert!(result.is_err());
                // A later request in this process cannot re-exchange the same
                // ambiguous generation even though it remains on disk.
                assert!(
                    session
                        .credential_with_exchange(&selected, &mut || false, |_| {
                            panic!("ambiguous generation exchanged twice")
                        })
                        .is_err()
                );
            }
            result
        });
        while !worker.is_finished() {
            let message = match runtime
                .state()
                .worker_rx
                .recv_timeout(Duration::from_millis(50))
            {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => panic!("worker mailbox: {error}"),
            };
            let crate::WorkerMessage::ChatGptPlanSecret(rpc) = message else {
                panic!("unexpected worker message");
            };
            let payload = match &rpc.op {
                ExtensionDataRequestOp::ReadFile { .. } => ExtensionDataResultPayload::Ok {
                    value: ExtensionDataValue::ReadFile {
                        contents: stored.clone(),
                    },
                },
                ExtensionDataRequestOp::CompareAndSwapFile {
                    expected_generation,
                    contents,
                    ..
                } => {
                    assert_eq!(
                        expected_generation,
                        &blake3::hash(&initial).to_hex().to_string()
                    );
                    if scenario == 3 {
                        // A removal won before CAS. Report a failed publication
                        // without installing any of the rotated bytes.
                        stored.clear();
                        ExtensionDataResultPayload::Error {
                            kind: tau_proto::ExtensionDataErrorKind::GenerationMismatch,
                            message: "fixture credential removed".into(),
                        }
                    } else {
                        stored = contents.clone();
                        ExtensionDataResultPayload::Ok {
                            value: ExtensionDataValue::CompareAndSwapFile,
                        }
                    }
                }
                _ => panic!("unexpected storage operation"),
            };
            runtime.state_mut().start_chatgpt_plan_secret(rpc);
            let ids = runtime
                .state()
                .credential_admission
                .chatgpt_plan
                .replies
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(ids.len(), 1);
            runtime
                .state_mut()
                .handle_extension_data_result(ExtensionDataResult {
                    request_id: ids[0].clone(),
                    result: payload,
                })
                .expect("main-loop reply");
        }
        let result = worker.join().expect("worker");
        assert_eq!(exchanges.load(Ordering::SeqCst), usize::from(scenario >= 2));
        match scenario {
            1 => assert_eq!(
                result
                    .expect("adopt winner")
                    .access_token()
                    .expect("bearer"),
                "authoritative"
            ),
            2 => {
                assert!(canceled.load(Ordering::SeqCst));
                assert_eq!(
                    result.expect("rotation").access_token().expect("bearer"),
                    "rotated"
                );
                assert_eq!(
                    Credential::decode(&stored)
                        .expect("saved")
                        .access_token()
                        .expect("saved bearer"),
                    "rotated"
                );
            }
            3 => {
                assert!(result.is_err());
                assert!(stored.is_empty());
            }
            5 => {
                assert!(result.is_err());
                assert!(
                    Credential::decode(&stored)
                        .expect("retained registration")
                        .access_token()
                        .is_err()
                );
            }
            _ => {
                assert!(result.is_err());
                assert_eq!(stored, initial);
            }
        }
    }
}
