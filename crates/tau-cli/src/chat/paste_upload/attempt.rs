use super::*;

/// Source, retry-safe transfer state, and the exact editor completion owner.
pub(super) struct Attempt {
    /// Current editor attempt identity (changes on explicit retry).
    pub(super) id: u64,
    /// Immutable session authority captured when the paste starts.
    pub(super) session: tau_proto::SessionId,
    /// Shared source identity lets explicit retry resume without changing
    /// bytes.
    pub(super) text: tau_cli_term::PasteContent,
    /// Existing chunk/offset/digest-verifying artifact state machine.
    pub(super) upload: tau_client::ArtifactUpload,
    /// Local editor receiving only reference or content-free failure.
    pub(super) handle: tau_cli_term::TermHandle,
    /// Preflight must succeed before allocating an upload.
    pub(super) available: bool,
    /// A failed attempt waits for explicit retry or discard.
    pub(super) paused: bool,
    /// Fixed deadline for this explicit attempt, not renewed by chunk progress.
    pub(super) deadline: Instant,
    /// Correlation checked again after result delivery races cancellation.
    pub(super) request: Option<ArtifactRequestId>,
    /// Current original in a validated bundle; finalized predecessors are never
    /// uploaded again on an explicit retry.
    pub(super) artifact_index: usize,
    /// Completed references withheld from the editor until every file finishes.
    pub(super) references: String,
}

impl Attempt {
    /// Binds existing paste metadata once, without interpreting the payload.
    pub(super) fn upload(
        text: &tau_cli_term::PasteContent,
        index: usize,
    ) -> tau_client::ArtifactUpload {
        let original = text.artifact(index).expect("validated original");
        tau_client::ArtifactUpload::with_metadata(
            original.bytes.to_vec(),
            original.filename.map(tau_proto::bounded_artifact_metadata),
            original.mime_type.map(tau_proto::bounded_artifact_metadata),
        )
        .expect("source and metadata bounded before upload")
    }
    /// Retains finalized references privately and advances to the next
    /// original.
    ///
    /// Cancellation can leave finalized immutable originals; it does not insert
    /// this partial list into the prompt or promise backing-store rollback.
    pub(super) fn finish_artifact(&mut self) -> Option<String> {
        let descriptor = self.upload.descriptor().expect("finalized original");
        if !self.references.is_empty() {
            self.references.push('\n');
        }
        self.references.push_str(&paste_reference_at(
            &descriptor.key,
            &self.text,
            self.artifact_index,
        ));
        self.artifact_index += 1;
        if self.artifact_index == self.text.artifact_count() {
            return Some(std::mem::take(&mut self.references));
        }
        self.upload = Self::upload(&self.text, self.artifact_index);
        self.request = None;
        None
    }
    /// Reports failure without forgetting retry-safe transfer identity.
    pub(super) fn fail(&mut self, error: impl ToString) {
        self.paused = true;
        self.request = None;
        self.handle
            .finish_paste_upload(self.id, Err(error.to_string()));
    }

    /// Writes one bounded operation and installs correlation before sending.
    pub(super) fn send_next(
        &mut self,
        writer: &WriterHandle,
        expected: &Mutex<Option<ArtifactRequestId>>,
    ) {
        if Instant::now() >= self.deadline {
            self.fail("upload timed out");
            return;
        }
        let op = if self.available {
            self.upload.next_op().expect("unfinished upload operation")
        } else {
            ArtifactOp::Available
        };
        let request = request(&self.session, op);
        self.request = Some(request.request_id.clone());
        *expected.lock().expect("paste correlation poisoned") = self.request.clone();
        if let Err(error) = send_frame(writer, &HarnessInputMessage::ArtifactRequest(request)) {
            *expected.lock().expect("paste correlation poisoned") = None;
            self.fail(error);
        }
    }

    /// Validates correlated progress, preserving state unchanged on failure.
    pub(super) fn accept(&mut self, result: ArtifactResult) -> Result<(), String> {
        let value = result.result.map_err(|error| error.to_string())?;
        if self.available {
            self.upload.accept(value).map_err(|error| error.to_string())
        } else if matches!(value, ArtifactValue::Done) {
            self.available = true;
            Ok(())
        } else {
            Err("invalid upload availability response".to_owned())
        }
    }

    /// Sends best-effort staging cleanup; committed originals remain immutable.
    pub(super) fn abort(&self, writer: &WriterHandle) {
        if let Some(op) = self.upload.abort_op() {
            let _ = send_frame(
                writer,
                &HarnessInputMessage::ArtifactRequest(request(&self.session, op)),
            );
        }
    }
}
