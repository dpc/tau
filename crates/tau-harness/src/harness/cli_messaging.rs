//! Exact-session local CLI ingress and ephemeral one-shot response
//! capabilities.
//!
//! This is same-user UI IPC, not cooperative inter-harness callback transport.
//! Ordinary receive publication owns durability; the socket owns only the
//! reply.

use super::*;

/// Socket-owned single response, buffered if submission races delivery ACK.
pub(super) struct CliReplyEndpoint {
    /// Live local socket owning this endpoint.
    pub(super) connection: tau_proto::ConnectionId,
    /// Whether the durable delivery acknowledgement has been enqueued.
    acknowledged: bool,
    /// An already-consumed response waiting behind its delivery
    /// acknowledgement.
    response: Option<String>,
}

impl Harness {
    /// Admit one CLI message without constructing a sender agent or session.
    pub(super) fn handle_cli_message(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request: tau_proto::CliMessageRequest,
    ) {
        if let Err(error) = self.queue_cli_message(client_id, request) {
            self.fail_cli_message(client_id, error);
        }
    }

    /// Reject an uncommitted CLI request and retire its response capability.
    pub(super) fn fail_cli_message(&mut self, client_id: &tau_proto::ConnectionId, reason: String) {
        self.peer_messaging
            .cli_replies
            .retain(|_, endpoint| &endpoint.connection != client_id);
        let _ = self.runtime_io.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::CliMessageResult(tau_proto::CliMessageResult::Rejected {
                reason,
            }),
        );
    }

    fn queue_cli_message(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request: tau_proto::CliMessageRequest,
    ) -> Result<(), String> {
        let admission = self.ui_runtime.exact_session_admissions.get(client_id);
        if !self.is_attached_socket_ui(client_id)
            || !admission.is_some_and(|admission| {
                admission.session_id == request.session_id
                    && admission.generation == self.session_runtime.current_session_generation
            })
            || request.session_id != self.session_runtime.current_session_id
        {
            return Err("CLI message requires exact-session UI admission".to_owned());
        }
        if self.peer_messaging.cli_connections.contains_key(client_id) {
            return Err("only one CLI message is allowed per connection".to_owned());
        }
        if request.message.len() > 64 * 1024 {
            return Err("CLI message exceeds the 64 KiB limit".to_owned());
        }
        if self.peer_messaging.pending_external_receive_acks.len() >= 16 {
            return Err("message receive commit queue is busy".to_owned());
        }
        // The label is explicitly a CLI origin, not an agent identity. The
        // separate opaque reply address cannot collide with valid agent ids.
        use rand::Rng as _;
        let nonce: String = (&mut self.agent_runtime.agent_registry.id_rng)
            .sample_iter(rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        let sender_id = crate::parse_agent_id(format!("cli-{nonce}"));
        let message_id = tau_proto::AgentMessageId::parse(format!("cli-{nonce}"))
            .expect("generated CLI message id");
        let (recipient_id, started, rate_admitted_at) = match &request.recipient {
            tau_proto::ExternalAgentMessageRecipient::Exact(agent_id) => {
                if self.agent_message_recipient_status(agent_id.as_str())
                    != AgentMessageRecipientStatus::Live
                {
                    return Err("recipient is unavailable, stopped, or unknown".to_owned());
                }
                (
                    agent_id.clone(),
                    false,
                    self.admit_peer_input(agent_id, request.message.len())?,
                )
            }
            tau_proto::ExternalAgentMessageRecipient::BareEntrypoint => self
                .resolve_peer_entrypoint_recipient(&message_id, request.message.len())
                .map_err(|error| error.into_diagnostic())?,
        };
        let cli_origin = if request.wait_response {
            let recipient = format!("!cli-{nonce}");
            self.peer_messaging.cli_replies.insert(
                recipient.clone(),
                CliReplyEndpoint {
                    connection: client_id.clone(),
                    acknowledged: false,
                    response: None,
                },
            );
            tau_proto::CliMessageOrigin::FinalResponse { recipient }
        } else {
            tau_proto::CliMessageOrigin::OneWay
        };
        self.peer_messaging.cli_connections.insert(
            client_id.clone(),
            self.session_runtime.current_session_generation,
        );
        let received = tau_proto::AgentMessageReceived {
            peer_review: None,
            message_id: message_id.clone(),
            sender_id,
            sender_session_id: None,
            cli_origin: Some(cli_origin),
            recipient_id: recipient_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            sender_notice: None,
            recipient_notice: None,
            message: request.message,
        };
        self.peer_messaging.pending_external_receive_acks.insert(
            message_id,
            PendingExternalReceiveAck {
                session_generation: self.session_runtime.current_session_generation,
                recipient_id,
                recipient: request.recipient,
                expected_receive: received.clone(),
                canceled: false,
                started,
                rate_admitted_at,
                completion: PendingPeerReceiveCompletion::Cli {
                    client_id: client_id.clone(),
                },
            },
        );
        self.publish_event(
            Some(harness_connection_id()),
            Event::AgentMessageReceived(received),
        );
        Ok(())
    }

    /// Consume a local CLI reply address once, without a fictitious sent fact.
    pub(crate) fn publish_cli_response(
        &mut self,
        recipient: &str,
        message: String,
    ) -> Result<(), String> {
        if message.len() > 64 * 1024 {
            return Err("CLI response exceeds the 64 KiB limit".to_owned());
        }
        let endpoint = self
            .peer_messaging
            .cli_replies
            .get_mut(recipient)
            .ok_or_else(|| {
                "CLI response address expired, already used, or belongs to another harness"
                    .to_owned()
            })?;
        let client_id = endpoint.connection.clone();
        if self.peer_messaging.cli_connections.get(&client_id)
            != Some(&self.session_runtime.current_session_generation)
        {
            return Err("CLI response address expired".to_owned());
        }
        if endpoint.response.is_some() {
            return Err("CLI response address already used".to_owned());
        }
        if !endpoint.acknowledged {
            endpoint.response = Some(message);
            return Ok(());
        }
        self.peer_messaging.cli_replies.remove(recipient);
        self.runtime_io
            .bus
            .send_to(
                &client_id,
                None,
                HarnessOutputMessage::CliMessageResult(tau_proto::CliMessageResult::Response {
                    message,
                }),
            )
            .map_err(|_| "CLI caller disconnected; response was not delivered".to_owned())?;
        Ok(())
    }

    /// Queue the durable ACK before exposing any already-submitted response.
    pub(super) fn acknowledge_cli_message(
        &mut self,
        connection: &tau_proto::ConnectionId,
        message_id: tau_proto::AgentMessageId,
        recipient_id: AgentId,
    ) {
        if self
            .runtime_io
            .bus
            .send_to(
                connection,
                None,
                HarnessOutputMessage::CliMessageResult(tau_proto::CliMessageResult::Delivered {
                    message_id,
                    recipient_id,
                }),
            )
            .is_err()
        {
            self.peer_messaging
                .cli_replies
                .retain(|_, endpoint| &endpoint.connection != connection);
            return;
        }
        let reply = self
            .peer_messaging
            .cli_replies
            .iter_mut()
            .find(|(_, endpoint)| &endpoint.connection == connection)
            .and_then(|(address, endpoint)| {
                endpoint.acknowledged = true;
                endpoint
                    .response
                    .take()
                    .map(|message| (address.clone(), message))
            });
        if let Some((address, message)) = reply {
            self.peer_messaging.cli_replies.remove(&address);
            let _ = self.runtime_io.bus.send_to(
                connection,
                None,
                HarnessOutputMessage::CliMessageResult(tau_proto::CliMessageResult::Response {
                    message,
                }),
            );
        }
    }
}
