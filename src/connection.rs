use std::io;
use std::io::prelude::*;
use std::io::{Error, ErrorKind};
use std::net::Ipv4Addr;

use mio::*;
use mio::tcp::*;

/// A stateful wrapper around a non-blocking stream. This connection is not
/// the SERVER connection. This connection represents the client connections
/// _accepted_ by the SERVER connection.
pub struct Connection {
    // handle to the peer stream
    sock: TcpStream,

    // token used to register with the poller
    pub token: Token,

    // set of events we are interested in
    interest: Ready,

    // track whether a connection needs to be (re)registered
    is_idle: bool,

    // track whether a connection is reset
    is_reset: bool,

    is_to_be_removed: bool,

    handshake_done: bool,

    addr: Ipv4Addr,
}

impl Connection {
    pub fn new(sock: TcpStream, token: Token, addr: Ipv4Addr) -> Connection {
        Connection {
            sock: sock,
            token: token,
            interest: Ready::hup(),
            is_idle: true,
            is_reset: false,
            is_to_be_removed: false,
            handshake_done: false,
            addr: addr,
        }
    }

    /// Handle read event from poller.
    ///
    /// The Handler must continue calling until None is returned.
    ///
    /// The receive buffer is sent back to `Server` so the message can be broadcast to all
    /// listening connections.
    pub fn readable(&mut self) -> io::Result<Option<()>> {
        // TODO
        // read from sock
        Ok(None)
    }

    /// Handle a writable event from the poller.
    ///
    /// Send one message from the send queue to the client. If the queue is empty, remove interest
    /// in write events.
    pub fn writable(&mut self) -> io::Result<()> {

        // TODO

        Ok(())
    }

    /// Queue an outgoing message to the client.
    ///
    /// This will cause the connection to register interests in write events with the poller.
    /// The connection can still safely have an interest in read events. The read and write buffers
    /// operate independently of each other.
    pub fn send_message(&mut self) -> io::Result<()> {
        trace!("connection send_message; token={:?}", self.token);

        if !self.interest.is_writable() {
            self.interest.insert(Ready::writable());
        }

        Ok(())
    }

    /// Register interest in read events with poll.
    ///
    /// This will let our connection accept reads starting next poller tick.
    pub fn register(&mut self, poll: &mut Poll) -> io::Result<()> {
        trace!("connection register; token={:?}", self.token);

        self.interest.insert(Ready::readable());

        poll.register(&self.sock,
                      self.token,
                      self.interest,
                      PollOpt::edge() | PollOpt::oneshot())
            .and_then(|()| {
                self.is_idle = false;
                Ok(())
            })
            .or_else(|e| {
                error!("Failed to reregister {:?}, {:?}", self.token, e);
                Err(e)
            })
    }

    /// Re-register interest in read events with poll.
    pub fn reregister(&mut self, poll: &mut Poll) -> io::Result<()> {
        trace!("connection reregister; token={:?}", self.token);

        poll.reregister(&self.sock,
                        self.token,
                        self.interest,
                        PollOpt::edge() | PollOpt::oneshot())
            .and_then(|()| {
                self.is_idle = false;
                Ok(())
            })
            .or_else(|e| {
                error!("Failed to reregister {:?}, {:?}", self.token, e);
                Err(e)
            })
    }

    pub fn mark_reset(&mut self) {
        trace!("connection mark_reset; token={:?}", self.token);

        self.is_reset = true;
    }

    #[inline]
    pub fn is_reset(&self) -> bool {
        self.is_reset
    }

    pub fn mark_idle(&mut self) {
        trace!("connection mark_idle; token={:?}", self.token);

        self.is_idle = true;
    }

    #[inline]
    pub fn is_idle(&self) -> bool {
        self.is_idle
    }

    pub fn mark_to_be_removed(&mut self) {
        trace!("connection mark_to_be_removed; token={:?}", self.token);

        self.is_to_be_removed = true;
    }

    #[inline]
    pub fn is_to_be_removed(&self) -> bool {
        self.is_to_be_removed
    }

    pub fn mark_handshake_done(&mut self) {
        trace!("connection handshake_done; token={:?}", self.token);

        self.handshake_done = true;
    }

    #[inline]
    pub fn is_handshake_done(&self) -> bool {
        self.handshake_done
    }

    #[inline]
    pub fn get_addr(&self) -> Ipv4Addr {
        self.addr
    }
}
