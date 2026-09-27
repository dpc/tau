//! Real worker callbacks and main-loop Secret routing with synthetic grants.

use std::io::Cursor;
use std::thread;

use tau_client::{ExtensionBuilder, ManualExtensionRuntime, TauExtension, TauExtensionRunner};
use tau_proto::{Configure, ExtensionDataResult, HarnessOutputMessage, HarnessOutputWriter};
use tau_provider_grok::credential::RefreshExchange;
use tau_provider_grok::oauth::{self, TokenResponse};

use super::*;
use crate::tests::grok::{credential, stage, stage_generation};
use crate::tests::{SharedTraceWriter, observation_test_runtime};

/// Concrete state keeps this fixture on the same non-Send SDK/main-loop path.
type Runtime = ProviderRuntime<fn(Option<&ProviderName>) -> BuiltinProviderProfiles>;

/// No provider handlers or network executors can run in this callback fixture.
struct TestExtension;

impl TauExtension for TestExtension {
    type State = Runtime;

    fn name(&self) -> &'static str {
        "grok-callback-test"
    }

    fn register(self, _builder: &mut ExtensionBuilder<Self::State>) {}
}

/// The exchange observes authoritative B, not the initially admitted A.
struct Exchange {
    /// Model a lost exchange response or a later ambiguous CAS acknowledgement.
    fail: bool,
}

impl RefreshExchange for Exchange {
    async fn exchange(&self, refresh: &str) -> Result<TokenResponse, oauth::Error> {
        assert_eq!(refresh, "refresh-authoritative-b");
        if self.fail {
            return Err(oauth::Error::Transport);
        }
        Ok(TokenResponse {
            access_token: "rotated".to_owned(),
            refresh_token: Some("rotated-refresh".to_owned()),
            expires_in: Some(3600),
        })
    }
}

/// Start the real worker resolver with only its one-shot network exchange
/// faked.
fn worker(
    runtime: &ManualExtensionRuntime<Runtime>,
    key: Key,
    observed: Credential,
    fail: bool,
) -> thread::JoinHandle<WorkerMessage> {
    let store = Store {
        key,
        tx: runtime.state().worker_tx.clone(),
        waker: runtime.waker(),
        observed_generation: Mutex::new(None),
        authoritative_generation: Mutex::new(None),
        changed_generation: Mutex::new(None),
    };
    thread::spawn(move || {
        let executor = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("executor");
        let result = executor.block_on(credential::resolve(
            &store,
            &Exchange { fail },
            &observed,
            RefreshReason::Expired,
            now_ms(),
            || false,
        ));
        store.completion(&observed, result)
    })
}

/// Correlate the callback through the production SDK dispatch and reply
/// handler.
fn reply(
    runtime: &mut ManualExtensionRuntime<Runtime>,
    rpc: Rpc,
    result: ExtensionDataResultPayload,
) {
    runtime.state_mut().start_grok_secret_request(rpc);
    let ids = runtime
        .state()
        .credential_admission
        .grok
        .rpcs
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 1);
    runtime
        .state_mut()
        .handle_extension_data_result(ExtensionDataResult {
            request_id: ids[0].clone(),
            result,
        })
        .expect("main-loop reply");
}

/// The actual Store callback, main-loop RPC and completion retain the exchange
/// generation even when admission A, exchange B and publication readback C
/// differ.
#[test]
fn grok_worker_secret_callbacks_suppress_exchanged_generation_not_readback() {
    for scenario in 0..3 {
        let fail_exchange = scenario == 0;
        let expired_winner = scenario == 2;
        let mut input = Vec::new();
        {
            let mut writer = HarnessOutputWriter::new(&mut input);
            writer
                .write_message(&HarnessOutputMessage::Configure(Configure {
                    purpose: Default::default(),
                    harness_protocol_version: None,
                    config: tau_proto::CborValue::Map(Vec::new()),
                    instance_name: "grok-callback-test".parse().expect("instance"),
                    tool_prefix: None,
                    state_dir: None,
                    secrets: Default::default(),
                    settings_files: Default::default(),
                }))
                .expect("configure");
            writer.flush().expect("flush");
        }
        let output = SharedTraceWriter::default();
        let mut runtime = TauExtensionRunner::new(TestExtension)
            .start_manual_loop_with_extension_data_state(Cursor::new(input), output, |_, client| {
                let mut state = observation_test_runtime();
                state.extension_data_client = Some(client);
                state.diagnostics.receipt.suppress_oauth_worker = true;
                state
            })
            .expect("runtime");
        let a = credential("admitted-a", 0);
        let b = credential("authoritative-b", 0);
        let c = credential("readback-c", 0);
        let c_bytes = serde_json::to_vec_pretty(&c).expect("noncanonical C");
        let c_generation = blake3::hash(&c_bytes);
        assert_ne!(c_generation, blake3::hash(&c.encode()));
        let key_a = stage(runtime.state_mut(), "initial", &a).expect("initial flight");
        let first_worker = worker(&runtime, key_a.clone(), a.clone(), fail_exchange);
        let WorkerMessage::GrokSecretRequest(rpc) = runtime
            .state()
            .worker_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first read")
        else {
            panic!("read callback");
        };
        assert!(
            matches!(&rpc.op, ExtensionDataRequestOp::ReadFile { path } if path == &key_a.path)
        );
        reply(
            &mut runtime,
            rpc,
            ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::ReadFile {
                    contents: b.encode(),
                },
            },
        );
        // An independent B admission already owns B's exchange. A must join,
        // not exchange B under A's key or spuriously require another login.
        let key = stage(runtime.state_mut(), "concurrent-b", &b).expect("B flight");
        assert_eq!(runtime.state().credential_admission.grok.flights.len(), 2);
        let WorkerMessage::GrokGenerationChanged {
            key: changed_key,
            credential,
            generation,
        } = first_worker.join().expect("first worker")
        else {
            panic!("changed generation must restage before exchange");
        };
        assert!(changed_key == key_a);
        runtime
            .state_mut()
            .finish_grok_generation_changed(changed_key, credential, generation);
        assert_eq!(runtime.state().credential_admission.grok.flights.len(), 1);
        assert!(runtime.state().credential_admission.grok.failed.is_empty());
        assert!(
            runtime
                .state()
                .credential_admission
                .admissions
                .iter()
                .all(|admission| admission.grok_refresh.as_ref() == Some(&key))
        );
        let second_worker = worker(&runtime, key.clone(), b.clone(), fail_exchange);
        for step in 0..if fail_exchange { 1 } else { 3 } {
            let WorkerMessage::GrokSecretRequest(rpc) = runtime
                .state()
                .worker_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("worker RPC")
            else {
                panic!("unexpected worker message");
            };
            let payload = match step {
                0 | 2 => {
                    assert!(
                        matches!(&rpc.op, ExtensionDataRequestOp::ReadFile { path } if path == &key.path)
                    );
                    ExtensionDataResultPayload::Ok {
                        value: ExtensionDataValue::ReadFile {
                            contents: if step == 0 {
                                b.encode()
                            } else {
                                c_bytes.clone()
                            },
                        },
                    }
                }
                1 => {
                    let ExtensionDataRequestOp::CompareAndSwapFile {
                        path,
                        expected_generation,
                        contents,
                    } = &rpc.op
                    else {
                        panic!("expected CAS");
                    };
                    assert!(path == &key.path);
                    assert_eq!(
                        expected_generation,
                        &blake3::hash(&b.encode()).to_hex().to_string()
                    );
                    assert_eq!(
                        Credential::decode(contents)
                            .expect("replacement")
                            .access_token(),
                        "rotated"
                    );
                    ExtensionDataResultPayload::Error {
                        kind: if expired_winner {
                            ExtensionDataErrorKind::GenerationMismatch
                        } else {
                            ExtensionDataErrorKind::Io
                        },
                        message: "ambiguous CAS acknowledgement".to_owned(),
                    }
                }
                _ => unreachable!(),
            };
            reply(&mut runtime, rpc, payload);
        }
        let WorkerMessage::GrokRefreshFinished {
            key: finished_key,
            observed_generation: observed,
            authoritative_generation,
            result,
        } = second_worker.join().expect("worker")
        else {
            panic!("exchange completion");
        };
        assert!(finished_key == key);
        assert_eq!(result.is_ok(), expired_winner);
        assert_eq!(
            observed.as_deref(),
            Some(blake3::hash(&b.encode()).to_hex().as_str())
        );
        runtime
            .state_mut()
            .finish_grok_refresh(key, observed, authoritative_generation, result);
        if expired_winner {
            assert_eq!(authoritative_generation, Some(c_generation));
            for id in ["c-next", "c-again"] {
                assert!(stage_generation(runtime.state_mut(), id, &c, c_generation).is_none());
                assert!(
                    runtime
                        .state()
                        .credential_admission
                        .admissions
                        .back()
                        .expect("C admission")
                        .profiles
                        .missing_login(&ProviderName::new("grok"))
                );
            }
            assert!(runtime.state().credential_admission.grok.is_idle());
            runtime.finish().expect("finish SDK");
            continue;
        }
        for id in ["b-next", "b-again"] {
            assert!(stage(runtime.state_mut(), id, &b).is_none());
            assert!(
                runtime
                    .state()
                    .credential_admission
                    .admissions
                    .back()
                    .expect("admission")
                    .profiles
                    .missing_login(&ProviderName::new("grok"))
            );
        }
        assert!(runtime.state().credential_admission.grok.is_idle());
        // A different readback must not overwrite the captured exchange hash.
        assert!(stage(runtime.state_mut(), "c-next", &c).is_some());
        runtime.finish().expect("finish SDK");
    }
}
