extern crate bip_bencode;
extern crate bip_metainfo;
extern crate bip_utracker;
extern crate chrono;
extern crate hyper;
extern crate slab;
extern crate url;

use self::bip_metainfo::MetainfoFile;
use self::bip_utracker::contact::CompactPeersV4;
use self::chrono::TimeZone;

use bit_vec::BitVec;
use mio::*;
use mio::channel::{Sender, Receiver, channel};
use mio::tcp::*;

use connection::Connection;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::net::SocketAddr;
use std::str;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

type Slab<T> = slab::Slab<T, Token>;
const BLOCK_SZ: usize = 16384;

#[allow(dead_code)]
pub struct BTClient {
    torrents: HashMap<usize, Arc<Mutex<Torrent>>>,
    torrent_roots: HashMap<usize, String>,
    peer_id: String, // peer_id or client id
    port: u16, // between 6881-6889
    next_id: usize,
    channels: HashMap<usize, Sender<Command>>,
}

impl BTClient {
    pub fn new(port: u16) -> BTClient {
        let now = SystemTime::now();
        let duration = now.duration_since(UNIX_EPOCH).unwrap();
        let peer_id = "-bittorrent-rs-".to_owned() + &format!("{}", duration.as_secs() % 100_000);
        debug!("peer_id: {}", peer_id);
        BTClient {
            torrents: HashMap::new(),
            torrent_roots: HashMap::new(),
            peer_id: peer_id,
            port: port,
            next_id: 0,
            channels: HashMap::new(),
        }
    }

    pub fn add(self: &mut BTClient, file: File) -> Result<(), String> {
        // TODO check if torrent already exists before insert
        let (tx, rx) = channel();
        let mut t = Torrent::new(file, self.peer_id.clone(), self.port, rx);
        let piece_len = t.metainfo.info().piece_length();
        let blocks_count_per_piece = (piece_len as usize) / BLOCK_SZ;
        let total_blocks = (blocks_count_per_piece as usize) *
                           (t.metainfo.info().pieces().count() as usize);

        t.block_bitmap.append(&mut vec![BitVec::from_elem(BLOCK_SZ, false); total_blocks]);

        let mut files = Vec::new();
        let mut pos: i64 = 0;
        for file in t.metainfo.info().files() {
            let length = file.length();
            let begin = pos;
            let end = begin + length - 1;
            pos = end + 1;
            let piece_begin = (begin / piece_len) as i64;
            let offset_begin = begin % piece_len;
            let piece_end = (end / piece_len) as i64;
            let offset_end = end % piece_len;

            let file = FileT {
                name: file.paths().next().unwrap_or("<unknown>").to_string(),
                start_piece: piece_begin as usize,
                start_offset: offset_begin,
                end_piece: piece_end as usize,
                end_offset: offset_end,
            };
            debug!("File: {:?}", file);
            files.push(file);
        }

        t.files.append(&mut files);
        self.torrent_roots.insert(self.next_id, t.root_name.clone());

        let torrent = Arc::new(Mutex::new(t));

        self.torrents.insert(self.next_id, torrent.clone());

        self.channels.insert(self.next_id, tx);
        self.next_id += 1;

        thread::spawn(move || torrent_loop(torrent));
        Ok(())
    }

    pub fn remove(self: &mut BTClient, id: usize) -> Result<usize, String> {
        self.torrents.remove(&id);
        self.torrent_roots.remove(&id);
        self.channels.remove(&id);
        Ok(self.torrents.len())
    }

    pub fn list(self: &BTClient) -> Vec<(usize, String)> {
        self.torrent_roots
            .iter()
            .map(|(id, root_name)| (*id, root_name.clone()))
            .collect()
    }

    pub fn start_download(self: &BTClient, id: usize) {
        self.channels[&id].send(Command::StartDownload).unwrap();
    }

    pub fn showfiles(self: &BTClient, id: usize) {
        self.channels[&id].send(Command::ListFiles).unwrap();
    }
}

#[allow(dead_code)]
struct Torrent {
    metainfo: MetainfoFile,
    root_name: String,
    peer_id: String,
    port: u16,

    piece_bitmap: BitVec,
    block_bitmap: Vec<BitVec>,

    files: Vec<FileT>,
    piece_nxt_req: usize,
    uploaded: usize,
    downloaded: usize,
    left: usize,
    num_seeders: usize,
    num_leachers: usize,

    tracker_info: RefCell<TrackerInfo>,

    // event loop specific
    command_rx: Receiver<Command>,
    conns: Slab<Connection>,
    token: Token,
    events: Events,
}

#[derive(Debug)]
struct TrackerInfo {
    peers: Vec<Peer>,
    interval: usize, // in seconds
    tracker_id: Option<String>,
}

impl TrackerInfo {
    fn new() -> TrackerInfo {
        TrackerInfo {
            peers: Vec::new(),
            interval: 0,
            tracker_id: None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
enum Command {
    ListFiles,
    StartDownload,
    StartSeed,
    StopDownload,
    StopSeed,
    Exit,
}

impl Torrent {
    fn new(mut file: File, peer_id: String, port: u16, rx: Receiver<Command>) -> Torrent {
        // byte vector for metainfo storage
        let mut bytes: Vec<u8> = Vec::new();
        file.read_to_end(&mut bytes).unwrap();

        let metainfo = MetainfoFile::from_bytes(bytes).unwrap();
        // TODO consider making this optional
        print_metainfo_overview(&metainfo);

        let root_name: String;
        if let Some(dir) = metainfo.info()
            .directory() {
            root_name = dir.to_owned();
        } else {
            root_name = metainfo.info()
                .files()
                .next()
                .unwrap()
                .paths()
                .next()
                .unwrap()
                .to_owned();
        }

        let piece_count = metainfo.info().pieces().count();
        let left_bytes = metainfo.info().files().fold(0, |acc, nex| acc + nex.length());

        Torrent {
            metainfo: metainfo,
            peer_id: peer_id,
            port: port,
            root_name: root_name,

            piece_bitmap: BitVec::from_elem(piece_count, false),
            block_bitmap: Vec::new(),

            piece_nxt_req: 0,
            files: Vec::new(),
            uploaded: 0,
            downloaded: 0,
            left: left_bytes as usize,
            num_seeders: 0,
            num_leachers: 0,

            tracker_info: RefCell::new(TrackerInfo::new()),

            command_rx: rx,
            // Give our server token a number much larger than our slab capacity. The slab used to
            // track an internal offset, but does not anymore.
            token: Token(10_000_000),

            // SERVER is Token(1), so start after that
            // we can deal with a max of 126 connections
            conns: Slab::with_capacity(128),

            // list of events from the poller that the server needs to process
            events: Events::with_capacity(1024),
        }
    }

    fn contact_tracker(self: &mut Torrent) {
        let metainfo = &self.metainfo;
        let info_hash = metainfo.info_hash();
        let info_hash_str = unsafe { str::from_utf8_unchecked(info_hash.as_ref()) };

        let mut request_url = url::Url::parse(metainfo.main_tracker().unwrap()).unwrap();
        request_url.query_pairs_mut()
            .append_pair("info_hash", info_hash_str)
            .append_pair("peer_id", &self.peer_id)
            .append_pair("port", &self.port.to_string())
            .append_pair("uploaded", &self.uploaded.to_string())
            .append_pair("downloaded", &self.downloaded.to_string())
            .append_pair("left", &self.left.to_string())
            .append_pair("compact", "1")
            .append_pair("event", "started")
            .append_pair("supportcrypto", "0");
        trace!("Request URL {:?}", request_url);

        let client = hyper::client::Client::new();
        let mut http_resp =
            client.get(request_url).header(hyper::header::Connection::close()).send().unwrap();
        trace!("{:?}", http_resp);

        let mut buffer = Vec::new();
        http_resp.read_to_end(&mut buffer).unwrap();
        let response = bip_bencode::Bencode::decode(&buffer).unwrap();
        trace!("{:?}", response);

        let (_, peer_ip_ports) = CompactPeersV4::from_bytes(response.dict()
                .unwrap()
                .lookup("peers")
                .unwrap()
                .bytes()
                .unwrap())
            .unwrap();

        let mut peers = Vec::new();
        trace!("Peer list received:");
        for ip_port in peer_ip_ports.iter() {
            trace!("{:?}", ip_port);
            peers.push(Peer::new(SocketAddr::V4(ip_port)));
        }

        let interval = response.dict().unwrap().lookup("interval").unwrap();
        trace!("interval: {:?}", interval);
        let interval: usize = 100;

        *self.tracker_info.borrow_mut() = TrackerInfo {
            peers: peers,
            interval: interval,
            tracker_id: None,
        }
    }

    #[allow(dead_code)]
    fn get_block_num(self: &mut Torrent, piece_num: usize) -> Result<usize, String> {
        let block = &self.block_bitmap[piece_num];
        let pos = block.iter().position(|x| !x).unwrap();
        Ok(pos)
    }

    #[allow(dead_code)]
    fn get_piece_num(self: &mut Torrent) -> Result<usize, String> {
        if self.piece_nxt_req <= self.metainfo.info().pieces().count() {
            let piece_num = self.piece_nxt_req;
            self.piece_nxt_req += 1;
            Ok(piece_num)
        } else {
            Err("No more pieces to request!".to_owned())
        }
    }

    #[allow(dead_code)]
    fn create_files(self: Torrent) -> Result<(), String> {
        for file in self.files {
            File::create(file.name).unwrap();
        }
        Ok(())
    }

    fn run(&mut self, poll: &mut Poll) -> io::Result<()> {

        try!(self.register(poll));

        info!("Server run loop starting...");
        loop {
            let cnt = try!(poll.poll(&mut self.events, None));

            let mut i = 0;

            // trace!("processing events... cnt={}; len={}",
            //        cnt,
            //        self.events.len());

            // Iterate over the notifications. Each event provides the token
            // it was registered with (which usually represents, at least, the
            // handle that the event is about) as well as information about
            // what kind of event occurred (readable, writable, signal, etc.)
            while i < cnt {
                let event = self.events.get(i).expect("Failed to get event");

                trace!("event={:?}; idx={:?}", event, i);
                self.ready(poll, event.token(), event.kind());

                i += 1;
            }

            self.tick(poll);
        }
    }

    fn ready(&mut self, poll: &mut Poll, token: Token, event: Ready) {
        trace!("{:?} event = {:?}", token, event);

        if event.is_error() {
            warn!("Error event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            return;
        }

        if event.is_hup() {
            trace!("Hup event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            println!("{:?}: client closed connection", token);
            return;
        }

        // We never expect a write event for our `Server` token . A write event for any other token
        // should be handed off to that connection.
        if event.is_writable() {
            trace!("Write event for {:?}", token);
            assert!(self.token != token, "Received writable event for Server");

            let conn = self.find_connection_by_token(token);

            if conn.is_reset() {
                info!("{:?} has already been reset", token);
                return;
            }

            conn.writable()
                .unwrap_or_else(|e| {
                    warn!("Write event failed for {:?}, {:?}", token, e);
                    conn.mark_reset();
                });
        }

        // A read event for our `Server` token means we are establishing a new connection. A read
        // event for any other token should be handed off to that connection.
        if event.is_readable() {
            trace!("Read event for {:?}", token);
            if self.token == token {
                self.accept_cmd(poll);
            } else {

                if self.find_connection_by_token(token).is_reset() {
                    info!("{:?} has already been reset", token);
                    return;
                }

                self.readable(token)
                    .unwrap_or_else(|e| {
                        warn!("Read event failed for {:?}: {:?}", token, e);
                        self.find_connection_by_token(token).mark_reset();
                    });
            }
        }

        if self.token != token {
            self.find_connection_by_token(token).mark_idle();
        }
    }

    fn tick(&mut self, event_loop: &mut Poll) {
        trace!("Handling end of tick");

        let mut reset_tokens = Vec::new();

        for c in self.conns.iter_mut() {
            if c.is_reset() {
                reset_tokens.push(c.token);
            } else if c.is_idle() {
                c.reregister(event_loop)
                    .unwrap_or_else(|e| {
                        warn!("Reregister failed {:?}", e);
                        c.mark_reset();
                        reset_tokens.push(c.token);
                    });
            }
        }

        for token in reset_tokens {
            match self.conns.remove(token) {
                Some(_c) => {
                    debug!("reset connection; token={:?}", token);
                }
                None => {
                    warn!("Unable to remove connection for {:?}", token);
                }
            }
        }
    }

    fn register(&mut self, poll: &mut Poll) -> io::Result<()> {
        poll.register(&self.command_rx,
                      self.token,
                      Ready::readable(),
                      PollOpt::edge())
            .or_else(|e| {
                error!("Failed to register server {:?}, {:?}", self.token, e);
                Err(e)
            })
    }

    fn accept_cmd(&mut self, poll: &mut Poll) {
        debug!("server accepting new command");

        let message = self.command_rx.try_recv().unwrap();
        debug!("{:?}", message);
        use self::Command::*;
        match message {
            ListFiles => {
                print_file_list(&self.metainfo);
            }
            StartDownload => {
                self.contact_tracker();
                info!("starting download");
                let peer_addrs: Vec<SocketAddr> =
                    self.tracker_info.borrow().peers.iter().map(|peer| peer.ip_port).collect();
                for (idx, addr) in peer_addrs.iter().enumerate() {
                    let stream = match TcpStream::connect(addr) {
                        Ok(s) => {
                            info!("connect() returned for {:?}", addr);
                            s
                        }
                        Err(e) => {
                            error!("Failed to accept new socket, {:?}", e);
                            return;
                        }
                    };

                    let token = match self.conns.vacant_entry() {
                        Some(entry) => {
                            debug!("registering {:?} with poller", entry.index());
                            let c = Connection::new(stream, entry.index());
                            entry.insert(c).index()
                        }
                        None => {
                            error!("Failed to insert connection into slab");
                            return;
                        }
                    };

                    match self.find_connection_by_token(token).register(poll) {
                        Ok(_) => {
                            info!("Successfully registered {:?} with poller", token);
                        }
                        Err(e) => {
                            error!("Failed to register {:?} connection with poller, {:?}",
                                   token,
                                   e);
                            self.conns.remove(token);
                            continue;
                        }
                    }
                    println!("{:?}: new peer connected; initiating handshake", token);
                    let info_hash = self.metainfo.info_hash();
                    let ih_ref = info_hash.as_ref();
                    let peer_id = self.peer_id.clone();
                    self.find_connection_by_token(token)
                        .send_handshake(ih_ref, peer_id)
                        .unwrap();
                    self.find_connection_by_token(token)
                        .send_interest()
                        .unwrap();
                    self.find_connection_by_token(token)
                        .send_piece_request(idx % peer_addrs.len(), 0, BLOCK_SZ)
                        .unwrap();
                }
            }
            _ => warn!("NOT YET IMPLEMENTED"),
        }
    }

    /// Forward a readable event to an established connection.
    ///
    /// Connections are identified by the token provided to us from the event loop. Once a read has
    /// finished, push the receive buffer into the all the existing connections so we can
    /// broadcast.
    fn readable(&mut self, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);

        while let Some(message) = try!(self.find_connection_by_token(token).readable()) {
            match message[0] {
                0 => info!("CHOKE"),
                1 => info!("UNCHOKE"),
                2 => info!("INTERESTED"),
                3 => info!("UNINTERESTED"),
                4 => info!("HAVE"),
                5 => {
                    info!("BITFIELD {:?}", message);
                    self.find_connection_by_token(token).piece_bitmap =
                        BitVec::from_bytes(&message[1..]);
                }
                6 => info!("REQUEST {:?}", message),
                7 => info!("PIECE {:?}", message),
                8 => info!("CANCEL {:?}", message),
                9 => info!("PORT {:?}", message),
                _ => info!("HANDSHAKE or unknown response {:?}", message),
            }
        }
        Ok(())
    }

    fn find_connection_by_token(&mut self, token: Token) -> &mut Connection {
        &mut self.conns[token]
    }
}

fn print_metainfo_overview(metainfo: &MetainfoFile) {
    let info = metainfo.info();
    let info_hash_hex = metainfo.info_hash()
        .as_ref()
        .iter()
        .map(|b| format!("{:02x}", b))
        .fold(String::new(), |mut acc, nex| {
            acc.push_str(&nex);
            acc
        });
    let utc_creation_date = metainfo.creation_date().map(|c| chrono::UTC.timestamp(c, 0));

    println!("------Metainfo File Overview-----");

    println!("InfoHash: {}", info_hash_hex);
    println!("Main Tracker: {}",
             metainfo.main_tracker().unwrap_or("<missing>"));
    println!("Comment: {}", metainfo.comment().unwrap_or("<missing>"));
    println!("Creator: {}", metainfo.created_by().unwrap_or("None"));
    println!("Creation Date: {:?}", utc_creation_date);

    println!("Directory: {}", info.directory().unwrap_or("None"));
    println!("Piece Length: {:?}", info.piece_length());
    println!("Number Of Pieces: {}", info.pieces().count());
    println!("Number Of Files: {}", info.files().count());
    println!("Total File Size: {}\n",
             info.files().fold(0, |acc, nex| acc + nex.length()));

    print_file_list(metainfo);
}

fn print_file_list(metainfo: &MetainfoFile) {
    let info = metainfo.info();

    println!("File List:");
    println!("Size (bytes)\tPath");
    println!("------------\t----------------------------------------------");
    for file in info.files() {
        println!("{:12}\t{}",
                 file.length(),
                 file.paths().next().unwrap_or("<unknown>"));
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct FileT {
    name: String,
    start_piece: usize,
    start_offset: i64,
    end_piece: usize,
    end_offset: i64,
}

#[allow(dead_code)]
#[derive(Debug)]
struct Peer {
    id: Option<String>, // peer_id
    ip_port: SocketAddr,
    piece_bitmap: BitVec,

    am_choking: bool,
    peer_choking: bool,
    am_interested: bool,
    peer_interested: bool,
}

impl Peer {
    fn new(ip_port: SocketAddr) -> Peer {
        Peer {
            id: None,
            ip_port: ip_port,
            piece_bitmap: BitVec::new(),

            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct TrackerRequest {
    info_hash: String,
    peer_id: String,
    port: usize, // port client is listening on, between 6881-6889
    uploaded: usize,
    downloaded: usize,
    left: usize,
    event: EventType, // TODO think about adding optional fields
}

#[allow(dead_code)]
#[derive(Debug)]
enum EventType {
    Started,
    Stopped,
    Completed,
}

fn torrent_loop(torrent_ref: Arc<Mutex<Torrent>>) {
    debug!("initiating torrent_loop for {:?}",
           torrent_ref.lock().unwrap().root_name);
    // Create a polling object that will be used by the Torrent to receive events
    let mut poll = Poll::new().expect("Failed to create Poll");
    let mut torrent = torrent_ref.lock().unwrap();
    torrent.run(&mut poll).expect("Failed to run server");
}
