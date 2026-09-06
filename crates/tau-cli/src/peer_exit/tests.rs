use super::*;

/// Socket EOF is not process exit. Even after dropping the opposite endpoint,
/// an exact handle for this still-running process must not report termination.
#[cfg(target_os = "linux")]
#[test]
fn closing_socket_does_not_confirm_peer_process_exit() {
    let (local, remote) = UnixStream::pair().expect("socket pair");
    let peer = match PeerExit::from_socket(&local) {
        Ok(peer) => peer,
        // Older supported kernels conservatively cannot confirm attached exit.
        Err(error) if error.raw_os_error() == Some(libc::ENOPROTOOPT) => return,
        Err(error) => panic!("pin socket peer: {error}"),
    };
    drop(remote);
    assert!(!peer.wait(Duration::ZERO).expect("poll current process"));
}
