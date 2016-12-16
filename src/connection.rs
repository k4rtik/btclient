use std::collections::vec_deque::VecDeque;
use std::io;
use std::io::prelude::*;
use std::io::{Error, ErrorKind};
use std::net::Ipv4Addr;
use std::str;
use std::sync::Arc;

use bit_vec::BitVec;
use byteorder::{ByteOrder, BigEndian};
use mio::*;
use mio::tcp::*;
use pnet::packet::Packet;

use packet::peer_protocol::*;

const PSTRLEN: u8 = 19;
const PSTR: &'static str = "BitTorrent protocol";
const RESERVED: [u8; 8] = [0u8; 8];
const HANDSHAKE_LEN: usize = 49 + PSTRLEN as usize;

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

    // track whether a read received `WouldBlock` and store the number of
    // byte we are supposed to read
    read_continuation: Option<u32>,

    send_queue: VecDeque<Arc<Vec<u8>>>,
    write_continuation: bool,

    pub piece_bitmap: BitVec,
    pub piece_buf: Vec<u8>,
}

impl Connection {
    pub fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,
            interest: Ready::hup(),
            is_idle: true,
            is_reset: false,
            is_to_be_removed: false,
            handshake_done: false,
            read_continuation: None,
            send_queue: VecDeque::new(),
            write_continuation: false,
            piece_bitmap: BitVec::new(),
            piece_buf: Vec::with_capacity(524288),
        }
    }

    /// Handle read event from poller.
    ///
    /// The Handler must continue calling until None is returned.
    ///
    /// The receive buffer is sent back to `Server` so the message can be broadcast to all
    /// listening connections.
    pub fn readable(&mut self) -> io::Result<Option<Vec<u8>>> {
        let msg_len = match try!(self.read_message_length()) {
            None => {
                return Ok(None);
            }
            Some(n) => n,
        };

        if msg_len == 0 {
            trace!("message is zero bytes; token={:?}", self.token);
            return Ok(None);
        }

        let msg_len = if self.handshake_done {
            msg_len as usize
        } else {
            49 + msg_len as usize - 1
        };

        debug!("Expected message length is {}", msg_len);
        let mut recv_buf: Vec<u8> = Vec::with_capacity(msg_len);
        unsafe {
            recv_buf.set_len(msg_len);
        }
        let len = {
            // UFCS: resolve "multiple applicable items in scope [E0034]" error
            let sock_ref = <TcpStream as Read>::by_ref(&mut self.sock);

            match sock_ref.take(msg_len as u64).read(&mut recv_buf) {
                Ok(n) => {
                    debug!("CONN : we read {} bytes", n);

                    if n < msg_len as usize {
                        return Err(Error::new(ErrorKind::InvalidData, "Did not read enough bytes"));
                    }

                    self.read_continuation = None;
                    n
                }
                Err(e) => {

                    if e.kind() == ErrorKind::WouldBlock {
                        debug!("CONN : read encountered WouldBlock");

                        // We are being forced to try again, but we already read the two bytes off of the
                        // wire that determined the length. We need to store the message length so we can
                        // resume next time we get readable.
                        self.read_continuation = Some(msg_len as u32);
                        return Ok(None);
                    } else {
                        error!("Failed to read buffer for token {:?}, error: {}",
                               self.token,
                               e);
                        return Err(e);
                    }
                }
            }
        };

        if len == msg_len && !self.handshake_done {
            self.mark_handshake_done();
        }

        Ok(Some(recv_buf.to_vec()))
    }

    fn read_message_length(&mut self) -> io::Result<Option<u32>> {
        if let Some(n) = self.read_continuation {
            return Ok(Some(n));
        }

        let len = if self.handshake_done { 4 } else { 1 };
        let mut buf: Vec<u8> = Vec::with_capacity(len);
        unsafe {
            buf.set_len(len);
        }

        let bytes = match self.sock.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    return Ok(None);
                } else {
                    return Err(e);
                }
            }
        };

        if bytes < len {
            warn!("Found message length of {} bytes", bytes);
            return Err(Error::new(ErrorKind::InvalidData, "Invalid message length"));
        }

        let msg_len = if self.handshake_done {
            BigEndian::read_u32(&buf)
        } else {
            buf[0] as u32
        };
        Ok(Some(msg_len))
    }


    /// Handle a writable event from the poller.
    ///
    /// Send one message from the send queue to the client. If the queue is empty, remove interest
    /// in write events.
    pub fn writable(&mut self) -> io::Result<()> {
        try!(self.send_queue
            .pop_front()
            .ok_or(Error::new(ErrorKind::Other, "Could not pop send queue"))
            .and_then(|buf| {
                /*
                match self.write_message_length(&buf) {
                    Ok(None) => {
                        // put message back into the queue so we can try again
                        self.send_queue.push_back(buf);
                        return Ok(());
                    }
                    Ok(Some(())) => {
                        ()
                    }
                    Err(e) => {
                        error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                        return Err(e);
                    }
                }*/

                trace!("writable(): {:?}", buf);

                match self.sock.write(&*buf) {
                    Ok(n) => {
                        debug!("CONN : we wrote {} bytes", n);
                        self.write_continuation = false;
                        Ok(())
                    }
                    Err(e) => {
                        if e.kind() == ErrorKind::WouldBlock {
                            debug!("writable(): client flushing buf; WouldBlock");

                            // put message back into the queue so we can try again
                            self.send_queue.push_back(buf);
                            self.write_continuation = true;
                            Ok(())
                        } else {
                            error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                            Err(e)
                        }
                    }
                }
            }));

        if self.send_queue.is_empty() {
            self.interest.remove(Ready::writable());
        }

        Ok(())
    }

    /// Queue an outgoing message to the client.
    ///
    /// This will cause the connection to register interests in write events with the poller.
    /// The connection can still safely have an interest in read events. The read and write buffers
    /// operate independently of each other.
    pub fn send_message(&mut self, message: Arc<Vec<u8>>) -> io::Result<()> {
        trace!("connection send_message; token={:?}", self.token);

        self.send_queue.push_back(message);

        if !self.interest.is_writable() {
            self.interest.insert(Ready::writable());
        }

        Ok(())
    }

    pub fn send_handshake(&mut self, info_hash: &[u8], peer_id: String) -> io::Result<()> {
        trace!("connection send_handshake; token={:?}", self.token);
        let info_hash_str = unsafe { str::from_utf8_unchecked(info_hash) };
        let mut pkt_buf = vec![0u8; HANDSHAKE_LEN];
        {
            let mut pkt = MutableHandshakePacket::new(&mut pkt_buf).unwrap();
            pkt.set_pstrlen(PSTRLEN);
            pkt.set_pstr(PSTR.as_bytes());
            pkt.set_reserved(&RESERVED);
            pkt.set_info_hash(info_hash_str.as_bytes());
            pkt.set_peer_id(peer_id.as_bytes());
        }

        let buf = Arc::new(pkt_buf);
        self.send_queue.push_back(buf);

        if !self.interest.is_writable() {
            self.interest.insert(Ready::writable());
        }

        Ok(())
    }

    pub fn send_interest(&mut self) -> io::Result<()> {
        trace!("connection send_interest; token={:?}", self.token);
        let mut pkt_buf = vec![0u8; 5];
        {
            let mut pkt = MutableControlPacket::new(&mut pkt_buf).unwrap();
            pkt.set_len(1);
            pkt.set_id(2);
        }

        let buf = Arc::new(pkt_buf);
        self.send_queue.push_back(buf);

        if !self.interest.is_writable() {
            self.interest.insert(Ready::writable());
        }

        Ok(())
    }

    pub fn send_piece_request(&mut self,
                              idx: usize,
                              begin: usize,
                              length: usize)
                              -> io::Result<()> {
        trace!("connection send_piece_request; token={:?}", self.token);
        let mut pkt_buf = vec![0u8; 17];
        {
            let mut pkt = MutableRequestPacket::new(&mut pkt_buf).unwrap();
            pkt.set_len(13);
            pkt.set_id(6);
            pkt.set_index(idx as u32);
            pkt.set_begin(begin as u32);
            pkt.set_length(length as u32);
        }

        let buf = Arc::new(pkt_buf);
        self.send_queue.push_back(buf);

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
        self.interest.insert(Ready::writable());

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
}
