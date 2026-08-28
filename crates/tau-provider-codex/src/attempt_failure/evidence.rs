use serde_json::Value;

use super::RedactedProviderDetail;
use super::redacted_detail::sanitize_live_detail;
use super::shape::project_shape;
use crate::canonical_identifier::CanonicalIdentifierFamily;

/// Work permitted while projecting one provider failure at the parser boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderEvidenceMode {
    /// Retain only the bounded candidate needed for live status.
    LiveOnly,
    /// Also compute persistent exact lengths and structural shape.
    Persistent,
}

impl ProviderEvidenceMode {
    fn is_persistent(self) -> bool {
        self == Self::Persistent
    }
}

/// Semantic WebSocket termination with no contradictory loose fields.
#[derive(Clone, Debug)]
pub(crate) enum WsTermination {
    /// The stream ended without a WebSocket close frame.
    CleanEof,
    /// The peer supplied a WebSocket close frame.
    CloseFrame {
        /// RFC 6455 close code, when supplied.
        code: Option<u16>,
        /// Provider-controlled reason used only by live-detail redaction.
        reason: Option<String>,
    },
}

/// Closed transport phase.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TransportPhase {
    /// Failure before the WebSocket upgrade completed.
    PreUpgrade,
    /// Failure while sending a request envelope.
    Send,
    /// Failure while receiving response events.
    ResponseStream,
}

impl TransportPhase {
    /// Return the schema-v1 label for this lifecycle phase.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::PreUpgrade => "pre_upgrade",
            Self::Send => "send",
            Self::ResponseStream => "response_stream",
        }
    }
}

/// Closed transport failure kind. Raw library displays never enter this type.
#[derive(Clone, Debug)]
pub(crate) enum TransportFailureKind {
    /// The peer closed the WebSocket or the stream reached EOF.
    WebSocketTermination(WsTermination),
    /// A rejected frame and its single byte-accounting fact.
    Frame(FrameFailure),
    /// The WebSocket reader failed.
    Read,
    /// The WebSocket writer failed.
    Send,
    /// A periodic transport-only WebSocket control-ping write failed.
    WebSocketControlPing,
    /// No provider frame arrived before the idle deadline.
    IdleTimeout,
    /// The WebSocket upgrade failed.
    Upgrade,
    /// The configured outbound route failed.
    Outbound,
}

/// One rejected provider frame used for both accounting and diagnostics.
#[derive(Clone, Debug)]
pub(crate) struct FrameFailure {
    /// Closed category of the rejected frame.
    kind: FrameFailureKind,
    /// Exact rejected-frame length used for transport accounting.
    response_bytes: usize,
}

impl FrameFailure {
    /// Construct a frame failure from the exact rejected frame length.
    #[must_use]
    pub(crate) fn new(kind: FrameFailureKind, response_bytes: usize) -> Self {
        Self {
            kind,
            response_bytes,
        }
    }

    /// Return the exact rejected frame length for transport accounting.
    #[must_use]
    pub(crate) fn response_bytes(&self) -> usize {
        self.response_bytes
    }
}

/// Closed rejected-frame category.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FrameFailureKind {
    /// A text frame was not valid JSON.
    MalformedText,
    /// A binary frame appeared where Responses JSON text was required.
    Binary,
}

impl TransportFailureKind {
    /// Return the schema-v1 label for this closed failure kind.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::WebSocketTermination(WsTermination::CleanEof) => "clean_eof",
            Self::WebSocketTermination(WsTermination::CloseFrame { .. }) => "websocket_close",
            Self::Frame(FrameFailure {
                kind: FrameFailureKind::MalformedText,
                ..
            }) => "malformed_text",
            Self::Frame(FrameFailure {
                kind: FrameFailureKind::Binary,
                ..
            }) => "binary_frame",
            Self::Read => "websocket_read",
            Self::Send => "websocket_send",
            Self::WebSocketControlPing => "websocket_control_ping",
            Self::IdleTimeout => "response_idle_timeout",
            Self::Upgrade => "websocket_upgrade",
            Self::Outbound => "outbound",
        }
    }
}

/// Opaque evidence observed at the parser or transport boundary.
#[derive(Clone, Debug)]
pub(crate) enum AttemptFailureEvidence {
    /// A structured Responses failure event.
    Provider(ProviderFailureEvidence),
    /// A locally classified transport observation.
    Transport {
        /// Lifecycle phase where the failure occurred.
        phase: TransportPhase,
        /// Whether the WebSocket upgrade had completed.
        established: bool,
        /// Closed transport kind with no raw library display.
        kind: TransportFailureKind,
        /// Request identifier from one allowlisted upgrade header.
        request_id: Option<String>,
        /// Whether an oversized upgrade identifier was omitted.
        identifiers_truncated: bool,
    },
}

/// Bounded structured facts projected from one terminal provider event.
#[derive(Clone, Debug)]
pub(crate) struct ProviderFailureEvidence {
    /// Bounded structured event type candidate.
    pub(super) event_type: Option<String>,
    /// Bounded structured canonical error-code candidate.
    pub(super) canonical_code: Option<String>,
    /// Bounded structured request-ID candidate.
    pub(super) request_id: Option<String>,
    /// Bounded structured response-ID candidate.
    pub(super) response_id: Option<String>,
    /// Bounded live prefix plus mode-dependent inspected prose lengths.
    pub(super) message: Option<ObservedText>,
    /// Bounded value-free terminal event shape.
    pub(super) shape: Option<Value>,
    /// Whether shape depth, nodes, entries, or collisions truncated.
    pub(super) shape_truncated: bool,
    /// Whether candidate bounds rejected an identifier.
    pub(super) identifiers_truncated: bool,
}

/// Provider prose retained as a bounded live prefix plus inspected lengths.
///
/// The lengths cover the full prose only in persistent mode. Live-only mode
/// stops immediately after proving that the prefix exceeded its display bound.
#[derive(Clone, Debug)]
pub(super) struct ObservedText {
    /// Prefix retained only for subsequent live-detail sanitation.
    pub(super) live_prefix: String,
    /// Whether the prefix contains the complete provider string.
    pub(super) live_prefix_complete: bool,
    /// Inspected UTF-8 bytes; exact only in persistent mode.
    pub(super) inspected_utf8_bytes: u64,
    /// Inspected Unicode scalars; exact only in persistent mode.
    pub(super) inspected_unicode_scalars: u64,
}

impl AttemptFailureEvidence {
    /// Project a raw provider event immediately at the parser boundary.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn provider(event: &Value) -> Self {
        Self::provider_with_mode(event, ProviderEvidenceMode::Persistent)
    }

    /// Project provider evidence, skipping persistent-only work when
    /// ineligible.
    #[must_use]
    pub(crate) fn provider_with_mode(event: &Value, mode: ProviderEvidenceMode) -> Self {
        let persistent = mode.is_persistent();
        let mut identifiers_truncated = false;
        let event_type = bounded_identifier_candidate(
            event.get("type").and_then(Value::as_str),
            &mut identifiers_truncated,
        );
        let error = event
            .get("response")
            .and_then(|response| response.get("error"))
            .or_else(|| event.get("error"));
        let identifier_family = CanonicalIdentifierFamily::from_provider_event(event);
        let canonical_code = bounded_identifier_candidate(
            identifier_family.classified(),
            &mut identifiers_truncated,
        );
        let message = error
            .and_then(|error| error.get("message").and_then(Value::as_str))
            .or_else(|| event.get("message").and_then(Value::as_str))
            .or_else(|| {
                event
                    .get("response")
                    .and_then(|response| response.pointer("/incomplete_details/reason"))
                    .and_then(Value::as_str)
            })
            .map(|value| observed_text(value, mode));
        let request_id = bounded_identifier_candidate(
            event
                .get("request_id")
                .and_then(Value::as_str)
                .or_else(|| {
                    event
                        .pointer("/response/request_id")
                        .and_then(Value::as_str)
                })
                .or_else(|| event.pointer("/error/request_id").and_then(Value::as_str)),
            &mut identifiers_truncated,
        );
        let response_id = bounded_identifier_candidate(
            event
                .pointer("/response/id")
                .and_then(Value::as_str)
                .or_else(|| event.get("id").and_then(Value::as_str))
                .or_else(|| event.get("response_id").and_then(Value::as_str)),
            &mut identifiers_truncated,
        );
        let shape = if persistent {
            project_shape(event)
        } else {
            super::shape::ShapeProjection {
                value: None,
                truncated: false,
            }
        };
        Self::Provider(ProviderFailureEvidence {
            event_type,
            canonical_code,
            request_id,
            response_id,
            message,
            shape: shape.value,
            shape_truncated: shape.truncated,
            identifiers_truncated,
        })
    }

    /// Construct locally classified transport evidence without an upgrade ID.
    #[must_use]
    pub(crate) fn transport(
        phase: TransportPhase,
        established: bool,
        kind: TransportFailureKind,
    ) -> Self {
        Self::Transport {
            phase,
            established,
            kind,
            request_id: None,
            identifiers_truncated: false,
        }
    }

    /// Construct a pre-upgrade failure with an allowlisted request-ID header.
    #[must_use]
    pub(crate) fn upgrade(request_id: Option<&str>, kind: TransportFailureKind) -> Self {
        let mut identifiers_truncated = false;
        Self::Transport {
            phase: TransportPhase::PreUpgrade,
            established: false,
            kind,
            request_id: bounded_identifier_candidate(request_id, &mut identifiers_truncated),
            identifiers_truncated,
        }
    }

    /// Return whether the failing operation dispatched a request envelope.
    #[must_use]
    pub(super) fn failure_was_dispatched(&self) -> bool {
        !matches!(
            self,
            Self::Transport {
                phase: TransportPhase::PreUpgrade,
                ..
            }
        )
    }

    /// Borrow the allowlisted upgrade request ID for transport-boundary tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn transport_request_id(&self) -> Option<&str> {
        match self {
            Self::Transport { request_id, .. } => request_id.as_deref(),
            Self::Provider(_) => None,
        }
    }

    /// Build an opaque live detail after applying profile-secret scrubbing.
    pub(crate) fn live_detail(
        &self,
        access_token: &str,
        account_id: Option<&str>,
    ) -> Option<RedactedProviderDetail> {
        let raw = match self {
            Self::Provider(provider) => match (
                provider.canonical_code.as_deref(),
                provider.message.as_ref().map(|message| {
                    if message.live_prefix_complete {
                        message.live_prefix.as_str()
                    } else {
                        "[redacted]"
                    }
                }),
            ) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                (None, Some(message)) => message.to_owned(),
                (Some(code), None) => format!("provider code {code}"),
                (None, None) => return None,
            },
            Self::Transport {
                kind: TransportFailureKind::WebSocketTermination(WsTermination::CleanEof),
                ..
            } => "WebSocket ended with clean EOF".to_owned(),
            Self::Transport {
                kind:
                    TransportFailureKind::WebSocketTermination(WsTermination::CloseFrame {
                        code,
                        reason,
                    }),
                ..
            } => match (code, reason.as_deref()) {
                (Some(code), Some(reason)) => format!("WebSocket closed with {code}: {reason}"),
                (Some(code), None) => format!("WebSocket closed with {code}"),
                (None, Some(reason)) => format!("WebSocket closed: {reason}"),
                (None, None) => "WebSocket closed".to_owned(),
            },
            Self::Transport { kind, .. } => match kind {
                TransportFailureKind::Frame(FrameFailure {
                    kind: FrameFailureKind::MalformedText,
                    ..
                }) => "Provider sent malformed WebSocket text".to_owned(),
                TransportFailureKind::Frame(FrameFailure {
                    kind: FrameFailureKind::Binary,
                    ..
                }) => "Provider sent an unexpected WebSocket binary frame".to_owned(),
                TransportFailureKind::Read => "WebSocket read failed".to_owned(),
                TransportFailureKind::Send => "WebSocket send failed".to_owned(),
                TransportFailureKind::WebSocketControlPing => {
                    "WebSocket control ping failed".to_owned()
                }
                TransportFailureKind::IdleTimeout => "Provider response timed out".to_owned(),
                TransportFailureKind::Upgrade => "WebSocket upgrade failed".to_owned(),
                TransportFailureKind::Outbound => "Provider connection failed".to_owned(),
                TransportFailureKind::WebSocketTermination(_) => unreachable!(),
            },
        };
        sanitize_live_detail(&raw, access_token, account_id)
    }
}

fn bounded_identifier_candidate(value: Option<&str>, truncated: &mut bool) -> Option<String> {
    let value = value?;
    if 1_024 < value.len() || 256 < value.chars().count() {
        *truncated = true;
        return None;
    }
    Some(value.to_owned())
}

fn observed_text(value: &str, mode: ProviderEvidenceMode) -> ObservedText {
    let persistent = mode.is_persistent();
    let mut live_prefix = String::new();
    let mut prefix_scalars = 0_usize;
    let mut unicode_scalars = 0_u64;
    let mut live_prefix_complete = true;
    for character in value.chars() {
        unicode_scalars = unicode_scalars.saturating_add(1);
        if prefix_scalars < 256 && live_prefix.len().saturating_add(character.len_utf8()) <= 1_024 {
            live_prefix.push(character);
            prefix_scalars += 1;
        } else {
            live_prefix_complete = false;
            if !persistent {
                break;
            }
        }
    }
    let bounded_utf8_bytes = u64::try_from(live_prefix.len()).unwrap_or(u64::MAX);
    ObservedText {
        live_prefix,
        live_prefix_complete,
        inspected_utf8_bytes: if persistent {
            u64::try_from(value.len()).unwrap_or(u64::MAX)
        } else {
            bounded_utf8_bytes
        },
        inspected_unicode_scalars: unicode_scalars,
    }
}
