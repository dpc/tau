use std::io::Write as _;
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::{ProtocolReadFailure, ProtocolReader, Readiness};

/// Keeps one absolute deadline across successive partial-record reads.
#[test]
fn partial_progress_preserves_original_deadline() {
    let (reader_stream, mut writer_stream) = UnixStream::pair().expect("protocol stream pair");
    reader_stream
        .set_nonblocking(true)
        .expect("nonblocking protocol reader");
    let expected_socket_fd = reader_stream.as_raw_fd();
    let (process, _process_hold) = UnixStream::pair().expect("quiet process descriptor pair");
    let expected_process_fd = process.as_raw_fd();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut reader = ProtocolReader::new(reader_stream);
    let mut supplied_deadlines = Vec::new();
    let mut readiness_calls = 0;

    let error = reader
        .read_line_with_readiness(
            process.as_fd(),
            deadline,
            |socket_fd, process_fd, supplied_deadline| {
                assert_eq!(socket_fd.as_raw_fd(), expected_socket_fd);
                assert_eq!(process_fd.as_raw_fd(), expected_process_fd);
                assert_eq!(supplied_deadline, deadline);
                supplied_deadlines.push(supplied_deadline);
                readiness_calls += 1;
                match readiness_calls {
                    1 => {
                        writer_stream.write_all(b"first").expect("first fragment");
                        Ok(Readiness::Socket)
                    }
                    2 => {
                        writer_stream
                            .write_all(b"-second")
                            .expect("second fragment");
                        Ok(Readiness::Socket)
                    }
                    3 => Ok(Readiness::DeadlineExpired),
                    call => panic!("unexpected readiness call {call}"),
                }
            },
        )
        .expect_err("scripted deadline expiry");

    assert!(matches!(error, ProtocolReadFailure::DeadlineExpired));
    assert_eq!(readiness_calls, 3);
    assert_eq!(supplied_deadlines, vec![deadline; 3]);
    assert_eq!(reader.buffered, b"first-second");
}
