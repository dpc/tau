//! Bounded cooperative declaration collection, without constructing a harness.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix_v1::event::{PollFd, PollFlags, Timespec};
use rustix_v1::fs::OFlags;
use rustix_v1::io::Errno;
use serde::Serialize;
use tau_proto::{Configure, HarnessInputMessage, HarnessOutputMessage};

use crate::CliError;

#[cfg(test)]
mod tests;

const MAX_EXTENSIONS: usize = 128;
const MAX_COLLECTION_BYTES: u64 = 32 * 1024 * 1024;
const MAX_EXTENSION_TIME: Duration = Duration::from_secs(10);
const CLEANUP_TIME: Duration = Duration::from_secs(1);
const IO_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Closed transport failures never echo child stderr, config, or parser
/// payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    CompleteDeclarations,
    Partial,
    Unsupported,
    ProtocolMismatch,
    InvalidProtocol,
    Unavailable,
    Deadline,
    CleanupFailed,
    Limit,
}

/// One configured origin; an inventory never asserts normal runtime readiness.
#[derive(Serialize)]
struct ExtensionPreview {
    /// Configured instance name, not a child-selected attribution.
    instance: String,
    /// Closed admission/collection result.
    outcome: Outcome,
    /// Typed declaration payload, retained only after protocol completion.
    inventory: Option<tau_proto::InspectionComplete>,
    /// State-owned provider settings were deliberately not inspected.
    state_settings_omitted: bool,
}

/// Deterministic declaration inventory, explicitly distinct from effective
/// tools.
#[derive(Serialize)]
struct Preview {
    /// Stable output schema, independent from extension protocol revisions.
    schema: u32,
    /// Human-readable boundary present even when no extension can be inspected.
    scope: &'static str,
    /// Credentials, service availability, policy and agent context are
    /// unverified.
    runtime_unverified: bool,
    /// Sorted configured extension origins, including unsupported entries.
    extensions: Vec<ExtensionPreview>,
    /// Names appearing in more than one declaration or visible-name slot.
    collisions: Vec<Collision>,
    /// Resolution failures use a closed summary rather than raw configuration.
    resolution_incomplete: bool,
}

/// Distinct routing namespaces must not collide in the report encoding.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum CollisionKind {
    Tool,
    Model,
}

/// A duplicate name in one closed routing namespace; no winner is selected.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Collision {
    /// Tool routing/visible slots or provider model routes.
    kind: CollisionKind,
    /// Exact declared name, without string-prefix encoding.
    name: String,
}

impl Preview {
    /// A failed configuration read has no discoverable origins, not zero
    /// configured extensions.
    fn new(resolution_incomplete: bool) -> Self {
        Self {
            schema: 1,
            scope: "declaration preview; not effective prompt/tools or runtime availability",
            runtime_unverified: true,
            extensions: Vec::new(),
            collisions: Vec::new(),
            resolution_incomplete,
        }
    }

    /// Normalize report-owned ordering without rewriting the peer's inventory.
    fn finalize(&mut self) {
        self.extensions
            .sort_by(|left, right| left.instance.cmp(&right.instance));
        self.collisions = collisions(&self.extensions);
    }

    /// Success claims only complete, unambiguous declaration collection.
    fn complete(&self) -> bool {
        !self.resolution_incomplete
            && self.collisions.is_empty()
            && self
                .extensions
                .iter()
                .all(|entry| entry.outcome == Outcome::CompleteDeclarations)
    }

    /// Emit the report even on closed collection/configuration failure.
    fn print(&mut self) -> Result<(), CliError> {
        self.finalize();
        serde_json::to_writer_pretty(std::io::stdout().lock(), self).map_err(io::Error::other)?;
        println!();
        if self.complete() {
            Ok(())
        } else {
            Err(CliError::Participant(
                "declaration preview is incomplete; see report".to_owned(),
            ))
        }
    }
}

/// Resolve permitted config inputs and collect only opt-in declaration
/// branches.
pub(crate) fn run(
    profile: Option<&str>,
    role_overrides: &[tau_config::settings::RoleCliOverride],
    extension_overrides: &[tau_config::settings::ExtensionCliOverride],
    environment_extensions: &[String],
    harness_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    let dirs = tau_config::settings::TauDirs {
        config_dir: tau_config::settings::config_dir(),
        state_dir: None,
    };
    let Ok(profile) = tau_config::settings::selected_profile_in(&dirs, profile) else {
        return Preview::new(true).print();
    };
    let Ok(settings) =
        tau_config::settings::load_harness_settings_with_profile_and_cli_overrides_in(
            &dirs,
            profile.as_ref(),
            role_overrides,
            harness_overrides,
        )
    else {
        return Preview::new(true).print();
    };
    let resolved = tau_harness::resolve_inspection_extensions(
        &settings,
        tau_harness::builtin_extensions(),
        environment_extensions,
        extension_overrides,
    );
    let mut extensions = resolved.extensions;
    extensions.sort_by(|left, right| left.name.cmp(&right.name));
    let mut preview = Preview::new(!resolved.unavailable.is_empty());
    preview.extensions.extend(
        resolved
            .unavailable
            .into_iter()
            .map(|entry| ExtensionPreview {
                instance: entry.name,
                outcome: Outcome::Unavailable,
                inventory: None,
                state_settings_omitted: entry.role.as_deref() == Some("provider"),
            }),
    );
    let mut remaining = MAX_COLLECTION_BYTES;
    for (index, extension) in extensions.iter().enumerate() {
        let result = if MAX_EXTENSIONS <= index || remaining == 0 {
            empty_preview(extension, Outcome::Limit)
        } else {
            collect(extension, dirs.config_dir.as_deref(), &mut remaining)
        };
        preview.extensions.push(result);
    }
    preview.print()
}

/// Keep skipped or failed origins in the inventory without retaining payloads.
fn empty_preview(extension: &tau_harness::ExtensionConfig, outcome: Outcome) -> ExtensionPreview {
    ExtensionPreview {
        instance: extension.name.clone(),
        outcome,
        inventory: None,
        state_settings_omitted: extension.role.as_deref() == Some("provider"),
    }
}

/// Spawn exactly the resolved command/cwd; no daemon, session or state setup.
fn collect(
    extension: &tau_harness::ExtensionConfig,
    config_root: Option<&std::path::Path>,
    remaining: &mut u64,
) -> ExtensionPreview {
    let mut command = Command::new(&extension.command);
    command
        .args(&extension.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = &extension.cwd {
        command.current_dir(cwd);
    }
    let Ok(mut child) = command.spawn() else {
        return empty_preview(extension, Outcome::Unavailable);
    };
    let deadline = Instant::now() + extension.startup_timeout.min(MAX_EXTENSION_TIME);
    let result = inspect_child(&mut child, extension, config_root, remaining, deadline);
    let cleaned = cleanup(&mut child);
    if !cleaned {
        return empty_preview(extension, Outcome::CleanupFailed);
    }
    match result {
        Ok(result) => result,
        Err(outcome) => empty_preview(
            extension,
            if *remaining == 0 {
                Outcome::Limit
            } else {
                outcome
            },
        ),
    }
}

/// Admit compatible explicit support before sending any Configure frame.
fn inspect_child(
    child: &mut Child,
    extension: &tau_harness::ExtensionConfig,
    config_root: Option<&std::path::Path>,
    remaining: &mut u64,
    deadline: Instant,
) -> Result<ExtensionPreview, Outcome> {
    let stdout = child.stdout.take().ok_or(Outcome::Unavailable)?;
    let stdin = child.stdin.take().ok_or(Outcome::Unavailable)?;
    inspect_connection(
        DeadlineIo::new(stdout, deadline).map_err(|_| Outcome::Unavailable)?,
        DeadlineIo::new(stdin, deadline).map_err(|_| Outcome::Unavailable)?,
        extension,
        config_root,
        remaining,
        deadline,
    )
}

/// Share exact admission and purpose lowering with in-memory wire oracles.
fn inspect_connection<R: Read, W: Write>(
    input: R,
    mut output: W,
    extension: &tau_harness::ExtensionConfig,
    config_root: Option<&std::path::Path>,
    remaining: &mut u64,
    deadline: Instant,
) -> Result<ExtensionPreview, Outcome> {
    let mut reader = tau_proto::HarnessInputReader::new(BudgetReader {
        inner: input,
        remaining,
    });
    let first = reader
        .read_message_with_size()
        .map_err(|_| protocol_failure(deadline))?
        .ok_or(Outcome::InvalidProtocol)?;
    let HarnessInputMessage::Hello(hello) = first.message else {
        return Err(Outcome::InvalidProtocol);
    };
    if hello.protocol_version.major != tau_proto::PROTOCOL_VERSION.major {
        return Err(Outcome::ProtocolMismatch);
    }
    if !hello.declaration_inspection {
        return Err(Outcome::Unsupported);
    }
    let provider = extension.role.as_deref() == Some("provider");
    let expected_kind = if provider {
        tau_proto::ClientKind::Provider
    } else {
        tau_proto::ClientKind::Tool
    };
    if hello.client_kind != expected_kind {
        return Err(Outcome::InvalidProtocol);
    }
    let settings_files = if provider {
        config_root
            .map(|root| {
                let root =
                    tau_config::settings::extension_provider_config_dir_of(root, &extension.name)
                        .map_err(|_| Outcome::Unavailable)?;
                tau_harness::inspection_settings_files(&root).map_err(|_| Outcome::Unavailable)
            })
            .transpose()?
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let configure = Configure {
        purpose: tau_proto::ConfigurePurpose::DeclarationInspection,
        config: tau_proto::CborValue::serialized(&extension.config)
            .map_err(|_| Outcome::Unavailable)?,
        instance_name: extension.name.parse().map_err(|_| Outcome::Unavailable)?,
        tool_prefix: extension.tool_prefix.clone(),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files,
    };
    let mut writer = tau_proto::HarnessOutputWriter::new(Vec::new());
    writer
        .write_message(&HarnessOutputMessage::Configure(configure))
        .map_err(|_| protocol_failure(deadline))?;
    let frame = writer.into_inner();
    if frame.len() as u64 > tau_proto::MAX_PROTOCOL_MESSAGE_BYTES {
        return Err(Outcome::Limit);
    }
    output
        .write_all(&frame)
        .map_err(|_| protocol_failure(deadline))?;
    output.flush().map_err(|_| protocol_failure(deadline))?;
    let completion = reader
        .read_message_with_size()
        .map_err(|_| protocol_failure(deadline))?
        .ok_or(Outcome::InvalidProtocol)?;
    let HarnessInputMessage::InspectionComplete(inventory) = completion.message else {
        return Err(Outcome::InvalidProtocol);
    };
    Ok(ExtensionPreview {
        instance: extension.name.clone(),
        outcome: if inventory.gaps.is_empty() && !provider {
            Outcome::CompleteDeclarations
        } else {
            Outcome::Partial
        },
        inventory: Some(inventory),
        state_settings_omitted: provider,
    })
}

/// Charge every read, including malformed frames and buffered trailing bytes.
struct BudgetReader<'a, R> {
    /// Pipe or in-memory wire fixture.
    inner: R,
    /// Shared collection budget across every child.
    remaining: &'a mut u64,
}

impl<R: Read> Read for BudgetReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = bytes.len().min(*self.remaining as usize);
        if count == 0 {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let read = self.inner.read(&mut bytes[..count])?;
        *self.remaining -= read as u64;
        Ok(read)
    }
}

/// Distinguish expiry without exposing parser diagnostics or raw frame bytes.
fn protocol_failure(deadline: Instant) -> Outcome {
    if Instant::now() >= deadline {
        Outcome::Deadline
    } else {
        Outcome::InvalidProtocol
    }
}

/// Bound cleanup even if an executable ignores EOF or retains protocol pipes.
fn cleanup(child: &mut Child) -> bool {
    drop(child.stdin.take());
    drop(child.stdout.take());
    if matches!(child.try_wait(), Ok(Some(_))) {
        return true;
    }
    let _ = child.kill();
    let deadline = Instant::now() + CLEANUP_TIME;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(IO_POLL_INTERVAL),
        }
    }
}

/// Nonblocking child pipes keep protocol decoding and writes within one
/// deadline.
struct DeadlineIo<T> {
    /// Owned pipe; dropping it closes the descriptor without joining a thread.
    inner: T,
    /// Absolute deadline shared by both directions of the exchange.
    deadline: Instant,
}

impl<T: AsFd> DeadlineIo<T> {
    /// Preserve existing file flags while making pipe I/O nonblocking.
    fn new(inner: T, deadline: Instant) -> io::Result<Self> {
        let flags = rustix_v1::fs::fcntl_getfl(&inner)?;
        rustix_v1::fs::fcntl_setfl(&inner, flags | OFlags::NONBLOCK)?;
        Ok(Self { inner, deadline })
    }
}

/// Retry only readiness waits; all other I/O errors retain normal semantics.
fn retry_io<T: AsFd, R>(
    inner: &mut T,
    deadline: Instant,
    events: PollFlags,
    mut operation: impl FnMut(&mut T) -> io::Result<R>,
) -> io::Result<R> {
    loop {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match operation(inner) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let timeout = Timespec::try_from(remaining).map_err(io::Error::other)?;
                let mut fds = [PollFd::new(&*inner, events)];
                match rustix_v1::event::poll(&mut fds, Some(&timeout)) {
                    Ok(0) => return Err(io::ErrorKind::TimedOut.into()),
                    Ok(_) => {}
                    Err(Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

impl<T: Read + AsFd> Read for DeadlineIo<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        retry_io(&mut self.inner, self.deadline, PollFlags::IN, |inner| {
            inner.read(bytes)
        })
    }
}

impl<T: Write + AsFd> Write for DeadlineIo<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        retry_io(&mut self.inner, self.deadline, PollFlags::OUT, |inner| {
            inner.write(bytes)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        retry_io(&mut self.inner, self.deadline, PollFlags::OUT, |inner| {
            inner.flush()
        })
    }
}

/// Detect routing and model-visible-name collisions without selecting winners.
fn collisions(extensions: &[ExtensionPreview]) -> Vec<Collision> {
    let mut seen = BTreeSet::new();
    let mut seen_models = BTreeSet::new();
    let mut collisions = BTreeSet::new();
    for extension in extensions {
        let Some(inventory) = &extension.inventory else {
            continue;
        };
        for registration in &inventory.tools {
            let tool = &registration.tool;
            let mut names = BTreeSet::from([tool.name.as_str()]);
            if let Some(alias) = &tool.model_visible_name {
                names.insert(alias.as_str());
            }
            for name in names {
                if !seen.insert(name) {
                    collisions.insert(Collision {
                        kind: CollisionKind::Tool,
                        name: name.to_owned(),
                    });
                }
            }
        }
        for provider in &inventory.providers {
            for model in &provider.models {
                if !seen_models.insert(&model.id) {
                    collisions.insert(Collision {
                        kind: CollisionKind::Model,
                        name: model.id.to_string(),
                    });
                }
            }
        }
    }
    collisions.into_iter().collect()
}
