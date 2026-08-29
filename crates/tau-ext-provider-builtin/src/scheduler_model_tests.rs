//! Deterministic command-trace model tests for the retry scheduler.

use std::collections::BTreeMap;
use std::env as path_std_env;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestCaseError, TestRunner};

use super::openai_tests::scheduled_job;
use super::*;

const PROVIDERS: [&str; 2] = ["limited", "healthy"];
const PROMPTS: [&str; 4] = ["ap-0", "ap-1", "ap-2", "ap-3"];
const PR_SEEDS: [u64; 8] = [
    0x7272_716d_7779,
    0x5eed_0000_0001,
    0x5eed_0000_0002,
    0x5eed_0000_0003,
    0x5eed_0000_0004,
    0x5eed_0000_0005,
    0x5eed_0000_0006,
    0x5eed_0000_0007,
];

/// One generated command owned by the synchronous retry scheduler.
#[derive(Clone, Debug)]
enum ModelCommand {
    /// Parks a prompt with an independent deadline and optional shared
    /// constraint.
    Schedule {
        /// Index into [`PROMPTS`].
        prompt: usize,
        /// Index into [`PROVIDERS`].
        provider: usize,
        /// Independent delay in virtual ticks.
        delay: u8,
        /// Optional `(generation, boundary ticks)` shared constraint.
        cooldown: Option<(u8, u8)>,
    },
    /// Extends one provider's shared cooldown using new evidence.
    Extend {
        /// Index into [`PROVIDERS`].
        provider: usize,
        /// New evidence generation.
        generation: u8,
        /// Boundary in ticks after the current time.
        delay: u8,
    },
    /// Releases one exact provider generation.
    Release {
        /// Index into [`PROVIDERS`].
        provider: usize,
        /// Generation claimed by the successful probe.
        generation: u8,
    },
    /// Transfers one exact delayed prompt for a manual retry.
    RetryNow {
        /// Index into [`PROMPTS`].
        prompt: usize,
        /// Generated control correlation nonce.
        request: u8,
    },
    /// Cancels one delayed prompt.
    Cancel {
        /// Index into [`PROMPTS`].
        prompt: usize,
    },
    /// Cancels all delayed prompts.
    CancelAll,
    /// Advances the monotonic virtual clock.
    Advance {
        /// Number of virtual ticks to advance.
        ticks: u16,
    },
}

/// Independently represented delayed ownership used as the test oracle.
#[derive(Clone, Debug)]
struct ReferenceEntry {
    /// Provider namespace.
    provider: usize,
    /// Prompt-local lower bound in virtual ticks.
    independent_due: u64,
    /// Effective lower bound in virtual ticks, excluding production jitter.
    shared_boundary: Option<u64>,
    /// Shared evidence generation constraining this entry.
    generation: Option<u64>,
    /// FIFO tie-breaker allocated when the prompt is first scheduled.
    sequence: u64,
}

/// Typed scheduler output expected from one model transition.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ExpectedAction {
    /// A timer returned one prompt to the provider loop.
    Due(usize),
    /// Cancellation consumed one or more delayed-count owners.
    Canceled {
        /// Prompt index returned for terminal cancellation.
        prompt: usize,
        /// Scheduler/API ownership entries consumed.
        delayed_count: usize,
    },
    /// Manual control returned the exact parked prompt, or reported it absent.
    Manual {
        /// Prompt returned from delayed ownership.
        prompt: Option<usize>,
        /// Prompt named by the control even when no owner was found.
        requested_prompt: usize,
        /// Correlation ID that must survive the control round trip.
        request_id: String,
    },
}

/// Small reference model for ownership, scope, and monotonic deadlines.
#[derive(Default)]
struct ReferenceModel {
    /// Current virtual tick.
    now: u64,
    /// Delayed prompt owners keyed by stable prompt index.
    delayed: BTreeMap<usize, ReferenceEntry>,
    /// Last allocated FIFO sequence.
    sequence: u64,
    /// Number of jobs transferred, canceled, or made due.
    removed: usize,
}

impl ReferenceModel {
    /// Applies one command and returns prompt indices whose ownership left
    /// delayed state.
    fn step(&mut self, command: &ModelCommand) -> Vec<ExpectedAction> {
        match *command {
            ModelCommand::Schedule {
                prompt,
                provider,
                delay,
                cooldown,
            } => {
                let independent_due = self.now + u64::from(delay);
                if self.delayed.contains_key(&prompt) {
                    self.delayed.remove(&prompt);
                    self.removed += 2;
                    return vec![ExpectedAction::Canceled {
                        prompt,
                        delayed_count: 2,
                    }];
                } else {
                    self.sequence = self.sequence.saturating_add(1);
                    let (generation, shared_boundary) = cooldown.map_or((None, None), |(g, d)| {
                        (Some(u64::from(g)), Some(self.now + u64::from(d)))
                    });
                    self.delayed.insert(
                        prompt,
                        ReferenceEntry {
                            provider,
                            independent_due,
                            shared_boundary,
                            generation,
                            sequence: self.sequence,
                        },
                    );
                }
                Vec::new()
            }
            ModelCommand::Extend {
                provider,
                generation,
                delay,
            } => {
                let boundary = self.now + u64::from(delay);
                for (prompt, entry) in &mut self.delayed {
                    if entry.provider != provider {
                        continue;
                    }
                    if entry.generation.is_none() {
                        entry.independent_due = entry
                            .shared_boundary
                            .map_or(entry.independent_due, |shared| {
                                entry.independent_due.max(shared + jitter_ticks(*prompt))
                            });
                    }
                    entry.shared_boundary = Some(boundary);
                    entry.generation = Some(u64::from(generation));
                }
                Vec::new()
            }
            ModelCommand::Release {
                provider,
                generation,
            } => {
                for entry in self.delayed.values_mut().filter(|entry| {
                    entry.provider == provider && entry.generation == Some(u64::from(generation))
                }) {
                    entry.shared_boundary = Some(self.now);
                    entry.generation = None;
                }
                Vec::new()
            }
            ModelCommand::RetryNow { prompt, request } => {
                let removed = self.delayed.remove(&prompt).map(|_| prompt);
                self.removed += usize::from(removed.is_some());
                vec![ExpectedAction::Manual {
                    prompt: removed,
                    requested_prompt: prompt,
                    request_id: format!("model-retry-{request}"),
                }]
            }
            ModelCommand::Cancel { prompt } => {
                let removed = self
                    .delayed
                    .remove(&prompt)
                    .map(|_| ExpectedAction::Canceled {
                        prompt,
                        delayed_count: 1,
                    });
                self.removed += usize::from(removed.is_some());
                removed.into_iter().collect()
            }
            ModelCommand::CancelAll => {
                let removed = self
                    .delayed
                    .keys()
                    .copied()
                    .map(|prompt| ExpectedAction::Canceled {
                        prompt,
                        delayed_count: 1,
                    })
                    .collect::<Vec<_>>();
                self.removed += removed.len();
                self.delayed.clear();
                removed
            }
            ModelCommand::Advance { ticks } => {
                self.now += u64::from(ticks);
                let mut due = self
                    .delayed
                    .iter()
                    .filter_map(|(prompt, entry)| {
                        let boundary = entry
                            .shared_boundary
                            .map_or(entry.independent_due, |shared| {
                                entry.independent_due.max(shared + jitter_ticks(*prompt))
                            });
                        (boundary <= self.now).then_some((*prompt, boundary, entry.sequence))
                    })
                    .collect::<Vec<_>>();
                due.sort_unstable_by_key(|(_, boundary, sequence)| (*boundary, *sequence));
                for (prompt, _, _) in &due {
                    self.delayed.remove(prompt);
                }
                self.removed += due.len();
                due.into_iter()
                    .map(|(prompt, _, _)| ExpectedAction::Due(prompt))
                    .collect()
            }
        }
    }
}

/// Returns the production jitter as whole model ticks (milliseconds).
fn jitter_ticks(prompt: usize) -> u64 {
    let id = PROMPTS[prompt];
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ 1;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    1 + hash % u64::try_from(RESET_BOUNDARY_JITTER_MAX.as_millis()).expect("jitter bound")
}

/// Builds the bounded command grammar used by fixed and scheduled seeds.
fn command_strategy() -> impl Strategy<Value = ModelCommand> {
    prop_oneof![
        5 => (
            0_usize..PROMPTS.len(),
            0_usize..PROVIDERS.len(),
            0_u8..20,
            proptest::option::of((0_u8..4, 0_u8..20)),
        ).prop_map(|(prompt, provider, delay, cooldown)| ModelCommand::Schedule {
            prompt, provider, delay, cooldown
        }),
        2 => (0_usize..PROVIDERS.len(), 0_u8..4, 0_u8..20)
            .prop_map(|(provider, generation, delay)| ModelCommand::Extend {
                provider, generation, delay
            }),
        2 => (0_usize..PROVIDERS.len(), 0_u8..4)
            .prop_map(|(provider, generation)| ModelCommand::Release {
                provider, generation
            }),
        2 => (0_usize..PROMPTS.len(), any::<u8>())
            .prop_map(|(prompt, request)| ModelCommand::RetryNow { prompt, request }),
        2 => (0_usize..PROMPTS.len()).prop_map(|prompt| ModelCommand::Cancel { prompt }),
        1 => Just(ModelCommand::CancelAll),
        3 => (0_u16..60_000).prop_map(|ticks| ModelCommand::Advance { ticks }),
    ]
}

/// Formats the exact minimized replay trace with stable line numbers.
fn format_trace(seed: u64, trace: &[ModelCommand]) -> String {
    let mut output = format!("scheduler model seed: {seed:#018x}\n");
    for (index, command) in trace.iter().enumerate() {
        let _ = writeln!(output, "{index:02}: {command:?}");
    }
    output
}

/// Drives the production synchronous state and reference model in lockstep.
fn check_trace(seed: u64, trace: &[ModelCommand]) -> Result<(), TestCaseError> {
    let epoch = Instant::now();
    let mut sut = RetrySchedulerState::default();
    let mut reference = ReferenceModel::default();
    let mut action_count = 0_usize;
    for (step, command) in trace.iter().enumerate() {
        let expected_removed = reference.step(command);
        let actions = match *command {
            ModelCommand::Schedule {
                prompt,
                provider,
                delay,
                cooldown,
            } => sut.step(SchedulerCommand::Schedule {
                independent_due: epoch + Duration::from_millis(reference.now + u64::from(delay)),
                cooldown: cooldown.map(|(generation, boundary)| CooldownConstraint {
                    generation: u64::from(generation),
                    boundary: epoch + Duration::from_millis(reference.now + u64::from(boundary)),
                }),
                job: Box::new(scheduled_job(PROMPTS[prompt], PROVIDERS[provider])),
            }),
            ModelCommand::Extend {
                provider,
                generation,
                delay,
            } => sut.step(SchedulerCommand::ExtendCooldown {
                provider: ProviderName::new(PROVIDERS[provider]),
                due: epoch + Duration::from_millis(reference.now + u64::from(delay)),
                generation: u64::from(generation),
            }),
            ModelCommand::Release {
                provider,
                generation,
            } => sut.step(SchedulerCommand::ReleaseCooldown {
                provider: ProviderName::new(PROVIDERS[provider]),
                generation: u64::from(generation),
                now: epoch + Duration::from_millis(reference.now),
            }),
            ModelCommand::RetryNow { prompt, request } => sut.step(SchedulerCommand::RetryNow {
                request_id: tau_proto::RetryPromptRequestId::parse(format!(
                    "model-retry-{request}"
                ))
                .expect("valid fixed request ID"),
                agent_prompt_id: PROMPTS[prompt]
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            }),
            ModelCommand::Cancel { prompt } => sut.step(SchedulerCommand::Cancel(
                PROMPTS[prompt]
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            )),
            ModelCommand::CancelAll => sut.step(SchedulerCommand::CancelAll),
            ModelCommand::Advance { .. } => {
                sut.advance(epoch + Duration::from_millis(reference.now))
            }
        };
        let removed = actions.iter().map(observe_action).collect::<Vec<_>>();
        let mut expected = expected_removed;
        let mut actual = removed;
        if matches!(command, ModelCommand::CancelAll) {
            // BinaryHeap::drain has no semantic order for broadcast cancellation.
            expected.sort_unstable();
            actual.sort_unstable();
        }
        prop_assert_eq!(
            actual,
            expected,
            "action mismatch at step {}\n{}",
            step,
            format_trace(seed, trace)
        );
        action_count += actions
            .iter()
            .map(|action| match action {
                RetrySchedulerAction::Canceled { delayed_count, .. } => *delayed_count,
                RetrySchedulerAction::Due(_) => 1,
                RetrySchedulerAction::Manual { job, .. } => usize::from(job.is_some()),
            })
            .sum::<usize>();
        prop_assert_eq!(
            sut.queue.len(),
            reference.delayed.len(),
            "ownership mismatch at step {}\n{}",
            step,
            format_trace(seed, trace)
        );
        prop_assert!(
            sut.queue.membership_is_exact(),
            "heap/index mismatch at step {}\n{}",
            step,
            format_trace(seed, trace)
        );
        prop_assert_eq!(
            action_count,
            reference.removed,
            "delayed-count mismatch at step {}\n{}",
            step,
            format_trace(seed, trace)
        );
        assert_deadlines(seed, trace, step, epoch, &sut, &reference)?;
    }
    Ok(())
}

/// Measures production queue visits across increasing queue sizes. The exact
/// counter keeps this benchmark deterministic: extend, release, and a present
/// cancel inspect every entry once, while an absent indexed cancel inspects
/// none.
#[test]
#[ignore = "descriptive performance benchmark"]
fn benchmark_retry_queue_bulk_mutation_scaling() {
    let epoch = Instant::now();
    let provider = ProviderName::new("limited");
    for queue_size in [1_024_usize, 4_096, 16_384] {
        let mut queue = RetryScheduleQueue::default();
        for index in 0..queue_size {
            assert!(
                queue
                    .schedule(
                        epoch + Duration::from_secs(1),
                        None,
                        scheduled_job(&format!("ap-bench-{index}"), provider.as_str()),
                    )
                    .is_ok(),
                "benchmark prompt IDs are unique"
            );
        }

        queue.extend_cooldown(&provider, epoch + Duration::from_secs(2), 7);
        queue.release_cooldown(&provider, 7, epoch + Duration::from_secs(3));
        let present = "ap-bench-0"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe benchmark prompt ID");
        assert_eq!(queue.cancel(&present).len(), 1);
        let absent = "ap-bench-absent"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe benchmark prompt ID");
        assert!(queue.cancel(&absent).is_empty());

        let visits = queue.take_mutation_work();
        eprintln!("retry_queue_bulk queue={queue_size} visits={visits}");
        assert_eq!(visits, queue_size * 3);
        assert!(queue.membership_is_exact());
    }
}

/// Preserves the production action kind and accounting for model comparison.
fn observe_action(action: &RetrySchedulerAction) -> ExpectedAction {
    match action {
        RetrySchedulerAction::Due(job) => ExpectedAction::Due(prompt_index(job)),
        RetrySchedulerAction::Canceled { job, delayed_count } => ExpectedAction::Canceled {
            prompt: prompt_index(job),
            delayed_count: *delayed_count,
        },
        RetrySchedulerAction::Manual {
            job,
            request_id,
            agent_prompt_id,
        } => ExpectedAction::Manual {
            prompt: job.as_ref().map(prompt_index),
            requested_prompt: PROMPTS
                .iter()
                .position(|candidate| *candidate == agent_prompt_id.as_str())
                .expect("known requested prompt"),
            request_id: request_id.as_str().to_owned(),
        },
    }
}

/// Maps one generated prompt job back to its bounded model index.
fn prompt_index(job: &PromptJob) -> usize {
    PROMPTS
        .iter()
        .position(|candidate| *candidate == job.agent_prompt_id.as_str())
        .expect("known generated prompt")
}

/// Checks scope, generation, and independent-deadline invariants.
fn assert_deadlines(
    seed: u64,
    trace: &[ModelCommand],
    step: usize,
    epoch: Instant,
    sut: &RetrySchedulerState,
    reference: &ReferenceModel,
) -> Result<(), TestCaseError> {
    for (id, provider, due) in sut.queue.deadlines() {
        let prompt = PROMPTS
            .iter()
            .position(|candidate| *candidate == id.as_str())
            .expect("known generated prompt");
        let entry = reference.delayed.get(&prompt).expect("reference owner");
        prop_assert_eq!(provider.as_str(), PROVIDERS[entry.provider]);
        let expected = entry
            .shared_boundary
            .map_or(entry.independent_due, |shared| {
                entry.independent_due.max(shared + jitter_ticks(prompt))
            });
        prop_assert_eq!(
            due,
            epoch + Duration::from_millis(expected),
            "deadline mismatch at step {}\n{}",
            step,
            format_trace(seed, trace)
        );
    }
    Ok(())
}

/// Runs one reproducible seed and lets proptest shrink failures to replayable
/// traces.
fn run_seed(seed: u64, cases: u32) {
    let config = Config {
        cases,
        max_shrink_iters: 10_000,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    let strategy = proptest::collection::vec(command_strategy(), 1..=48);
    if let Err(error) = runner.run(&strategy, |trace| check_trace(seed, &trace)) {
        panic!("scheduler trace failed for seed {seed:#018x}: {error}");
    }
}

/// Protects typed ownership conservation, exact control correlation, scoped
/// generation release, and deadline preservation with fixed bounded seeds,
/// exact replay, and shrinking on every PR.
#[test]
fn scheduler_reference_model_fixed_command_traces() {
    let cases = match std::env::var("TAU_SCHEDULER_MODEL_CASES") {
        Ok(value) => value
            .parse::<u32>()
            .ok()
            .filter(|cases| *cases >= 32)
            .unwrap_or_else(|| panic!("TAU_SCHEDULER_MODEL_CASES must be at least 32")),
        Err(path_std_env::VarError::NotPresent) => 32,
        Err(error) => panic!("invalid TAU_SCHEDULER_MODEL_CASES: {error}"),
    };
    for seed in PR_SEEDS {
        run_seed(seed, cases);
    }
    eprintln!(
        "scheduler model completed {} scenarios across {} fixed seeds",
        usize::try_from(cases).expect("u32 fits usize") * PR_SEEDS.len(),
        PR_SEEDS.len()
    );
}

/// Preserves the minimized scheduler incident shape: stale generations remain
/// parked and a valid probe eventually wakes eligible peers.
#[test]
fn minimized_quota_release_trace_is_replayable() {
    let trace = vec![
        ModelCommand::Schedule {
            prompt: 0,
            provider: 0,
            delay: 0,
            cooldown: Some((1, 200)),
        },
        ModelCommand::Schedule {
            prompt: 1,
            provider: 0,
            delay: 40,
            cooldown: Some((2, 200)),
        },
        ModelCommand::Release {
            provider: 0,
            generation: 1,
        },
        ModelCommand::Advance { ticks: 100 },
        ModelCommand::Release {
            provider: 0,
            generation: 2,
        },
        ModelCommand::Advance { ticks: u16::MAX },
    ];
    check_trace(PR_SEEDS[0], &trace).expect("saved rrqmwy model trace");
}

/// Locks the two timer/manual ownership orderings and duplicate-command
/// behavior: either order transfers a delayed AP at most once.
#[test]
fn timer_manual_and_duplicate_races_have_one_owner() {
    for (seed, trace) in [
        (
            PR_SEEDS[1],
            vec![
                ModelCommand::Schedule {
                    prompt: 0,
                    provider: 0,
                    delay: 0,
                    cooldown: None,
                },
                ModelCommand::RetryNow {
                    prompt: 0,
                    request: 1,
                },
                ModelCommand::Advance { ticks: 1 },
                ModelCommand::RetryNow {
                    prompt: 0,
                    request: 2,
                },
            ],
        ),
        (
            PR_SEEDS[2],
            vec![
                ModelCommand::Schedule {
                    prompt: 0,
                    provider: 0,
                    delay: 0,
                    cooldown: None,
                },
                ModelCommand::Advance { ticks: 1 },
                ModelCommand::RetryNow {
                    prompt: 0,
                    request: 3,
                },
                ModelCommand::Cancel { prompt: 0 },
            ],
        ),
        (
            PR_SEEDS[3],
            vec![
                ModelCommand::Schedule {
                    prompt: 0,
                    provider: 0,
                    delay: 10,
                    cooldown: None,
                },
                ModelCommand::Schedule {
                    prompt: 0,
                    provider: 1,
                    delay: 20,
                    cooldown: Some((2, 30)),
                },
            ],
        ),
    ] {
        check_trace(seed, &trace).expect("saved ownership race trace");
    }
}
