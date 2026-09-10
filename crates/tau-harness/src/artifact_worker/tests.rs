use tau_proto::ArtifactOp;

use super::*;

fn request(id: &str) -> ArtifactRequest {
    ArtifactRequest {
        request_id: id.parse().expect("valid request"),
        expected_session_id: "session".parse().expect("session identifier"),
        op: ArtifactOp::Available,
    }
}

/// The finite request budget includes queued completed responses, preventing a
/// stalled central loop from turning worker completion into an unbounded queue.
#[test]
fn artifact_completion_retains_admission_until_consumption() {
    let temp = tempfile::tempdir().expect("private root");
    let (tx, rx) = mpsc::channel();
    let mut worker = ArtifactWorker::start(temp.path(), tx).expect("artifact worker");
    let connection = crate::test_connection_id("one");
    for _ in 0..REQUEST_LIMIT {
        worker
            .submit(
                "tool/session".to_owned(),
                connection.clone(),
                request("request"),
            )
            .expect("bounded admission");
    }
    let mut completed = Vec::new();
    for _ in 0..REQUEST_LIMIT {
        completed.push(
            rx.recv_timeout(Duration::from_secs(3))
                .expect("worker completion"),
        );
    }
    assert_eq!(
        worker.submit(
            "tool/session".to_owned(),
            connection.clone(),
            request("overflow")
        ),
        Err(ArtifactError::Busy)
    );
    completed.pop();
    worker
        .submit("tool/session".to_owned(), connection, request("recovered"))
        .expect("recovered capacity");
    assert!(rx.recv_timeout(Duration::from_secs(3)).is_ok());
    assert!(
        !temp.path().join("artifacts").exists(),
        "availability performs no filesystem probe"
    );
}
