use super::{escape_field, stream_output, write_output};

/// Line-oriented identifiers escape record separators and terminal controls.
#[test]
fn hostile_field_stays_on_one_ansi_control_safe_line() {
    assert_eq!(
        escape_field("line\ncarriage\r\u{1b}[31m"),
        "line\\ncarriage\\r\\u{1b}[31m"
    );
}

/// Streaming a privately staged agent trace uses the same successful
/// broken-pipe semantics as small line-oriented output.
#[test]
fn streamed_broken_pipe_is_success() {
    stream_output(&mut std::io::sink(), |_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed pipeline",
        ))
    })
    .expect("streamed broken pipe is successful");
}

/// Closing a pipeline early is a successful CLI write rather than a panic or
/// user-visible failure.
#[test]
fn broken_pipe_is_success() {
    struct BrokenPipe;

    impl std::io::Write for BrokenPipe {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed pipeline",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    write_output(&mut BrokenPipe, "session\n").expect("broken pipe is successful");
}
