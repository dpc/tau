use super::*;

/// Dropping before any adapter connection must unblock accept and join fully.
#[test]
fn scripted_server_drop_unblocks_preconnect_failure() {
    let server = ScriptedServer::spawn();
    drop(server);
}

/// Dropping during a partial body must interrupt the blocked exact body read.
#[test]
fn scripted_server_drop_unblocks_partial_request() {
    let server = ScriptedServer::spawn();
    let mut client = TcpStream::connect(server.address).expect("connect fixture");
    client
        .write_all(b"POST /responses HTTP/1.1\r\nContent-Length: 4\r\n\r\nxy")
        .expect("partial request body");
    drop(server);
}
