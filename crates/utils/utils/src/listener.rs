use std::net::{SocketAddr, TcpListener};
// Adapted from /actix-web-4.9.0/src/server.rs create_listener
// This is required as we need to access the TcpListener directly to figure out what port we've been assigned
// if randomisation (requested port 0) is used.
pub fn create_listener(addr: SocketAddr) -> eyre::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let backlog: i32 = 1024;
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    // need this so application restarts can pick back up the same port without suffering from time-wait
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(backlog)?;
    let listener = TcpListener::from(socket);
    Ok(listener)
}
