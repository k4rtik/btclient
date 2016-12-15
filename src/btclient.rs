extern crate bip_metainfo;
extern crate bip_bencode;
extern crate bip_utracker;
extern crate bit_vec;
extern crate chrono;
extern crate hyper;
extern crate url;

use self::bip_metainfo::MetainfoFile;
use self::bip_utracker::contact::CompactPeersV4;
use self::bit_vec::BitVec;
use self::chrono::TimeZone;

use pnet::packet::Packet;

use packet::peer_protocol::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::prelude::*;
use std::net::SocketAddrV4;
use std::net::TcpStream;
use std::str;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH, Duration};

const BLOCK_SZ: usize = 16384;

#[allow(dead_code)]
#[derive(Debug)]
pub struct BTClient {
    torrents: HashMap<usize, Arc<Mutex<Torrent>>>,
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
            peer_id: peer_id,
            port: port,
            next_id: 0,
            channels: HashMap::new(),
        }
    }

    pub fn add(self: &mut BTClient, file: fs::File) -> Result<(), String> {
        // TODO check if torrent already exists before insert
        let mut t = Torrent::new(file, self.peer_id.clone());
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
            let end = begin + length;
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
        let torrent = Arc::new(Mutex::new(t));

        self.torrents.insert(self.next_id, torrent.clone());

        let (tx, rx) = channel();
        self.channels.insert(self.next_id, tx);
        self.next_id += 1;

        thread::spawn(move || torrent_loop(rx, torrent));
        Ok(())
    }

    pub fn remove(self: &mut BTClient, id: usize) -> Result<usize, String> {
        self.torrents.remove(&id);
        Ok(self.torrents.len())
    }

    pub fn list(self: &BTClient) -> Vec<(usize, String)> {
        self.torrents
            .iter()
            .map(|(id, torrent)| {
                let torrent = torrent.lock().unwrap();
                (*id, torrent.root_name.clone())
            })
            .collect()
    }

    pub fn start_download(self: &BTClient, id: usize) {
        let mut torrent = self.torrents[&id].lock().unwrap();
        let tracker_info = torrent.contact_tracker(self.port);
        trace!("{:?}", tracker_info);
        debug!("Found {} peers", tracker_info.peers.len());
        *torrent.tracker_info.borrow_mut() = tracker_info;

        self.channels[&id].send(Command::StartDownload).unwrap();
    }

    pub fn showfiles(self: &BTClient, id: usize) {
        let torrent = self.torrents[&id].lock().unwrap();
        print_file_list(&torrent.metainfo);
    }
}

#[derive(Debug)]
struct Torrent {
    metainfo: MetainfoFile,
    root_name: String,
    peer_id: String,

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
    StartDownload,
    StartSeed,
    StopDownload,
    StopSeed,
    Exit,
}

impl Torrent {
    fn new(mut file: fs::File, peer_id: String) -> Torrent {
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
        }
    }

    fn contact_tracker(self: &Torrent, port: u16) -> TrackerInfo {
        let metainfo = &self.metainfo;
        let info_hash = metainfo.info_hash();
        let info_hash_str = unsafe { str::from_utf8_unchecked(info_hash.as_ref()) };

        let mut request_url = url::Url::parse(metainfo.main_tracker().unwrap()).unwrap();
        request_url.query_pairs_mut()
            .append_pair("info_hash", info_hash_str)
            .append_pair("peer_id", &self.peer_id)
            .append_pair("port", &port.to_string())
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
        trace!("{:?}", peer_ip_ports);

        let mut peers = Vec::new();
        trace!("Peer list received:");
        for ip_port in peer_ip_ports.iter() {
            trace!("{:?}", ip_port);
            peers.push(Peer::new(ip_port));
        }

        let interval = response.dict().unwrap().lookup("interval").unwrap();
        trace!("interval: {:?}", interval);
        let interval: usize = 100;

        TrackerInfo {
            peers: peers,
            interval: interval,
            tracker_id: None,
        }
    }

    fn get_block_num(self: &mut Torrent, piece_num: usize) -> Result<usize, String> {
        let block = &self.block_bitmap[piece_num];
        let pos = block.iter().position(|x| !x).unwrap();
        Ok(pos)
    }

    fn get_piece_num(self: &mut Torrent) -> Result<usize, String> {
        if self.piece_nxt_req <= self.metainfo.info().pieces().count() {
            let piece_num = self.piece_nxt_req;
            self.piece_nxt_req += 1;
            Ok(piece_num)
        } else {
            Err("No more pieces to request!".to_owned())
        }
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
    ip_port: SocketAddrV4,

    am_choking: bool,
    peer_choking: bool,
    am_interested: bool,
    peer_interested: bool,
}

impl Peer {
    fn new(ip_port: SocketAddrV4) -> Peer {
        Peer {
            id: None,
            ip_port: ip_port,
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

const PSTRLEN: u8 = 19;
const PSTR: &'static str = "BitTorrent protocol";
const RESERVED: [u8; 8] = [0u8; 8];
const HANDSHAKE_LEN: usize = 49 + PSTRLEN as usize;

fn handshake_peer(peer: &Peer, info_hash: &[u8], peer_id: &str) -> bool {
    let info_hash_str = unsafe { str::from_utf8_unchecked(info_hash) };
    let mut pkt_buf = vec![0u8; HANDSHAKE_LEN];
    let mut pkt = MutableHandshakePacket::new(&mut pkt_buf).unwrap();
    pkt.set_pstrlen(PSTRLEN);
    pkt.set_pstr(PSTR.as_bytes());
    pkt.set_reserved(&RESERVED);
    pkt.set_info_hash(info_hash_str.as_bytes());
    pkt.set_peer_id(peer_id.as_bytes());
    trace!("pkt: {:?}", pkt);
    // XXX: connect() can block everyone else for a minute or more
    match TcpStream::connect(peer.ip_port) {
        Ok(mut stream) => {
            debug!("connect() success: {:?}", peer.ip_port);
            stream.set_write_timeout(Some(Duration::from_millis(100))).unwrap();
            match stream.write(pkt.packet()) {
                Ok(_) => {
                    debug!("write() success: {:?}", peer.ip_port);
                    stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                    let mut buf_read = vec![0; 2048];
                    match stream.read(&mut buf_read) {
                        Ok(bytes_read) => {
                            debug!("read() success: {:?}", peer.ip_port);
                            debug!("Bytes read: {:?}", bytes_read);
                            trace!("peer_id: {:?}", String::from_utf8_lossy(&buf_read[0..68]));
                            str::from_utf8(&buf_read[1..20]).unwrap() == PSTR
                        }
                        Err(e) => {
                            error!("read() failed: {:?} {:?}", peer.ip_port, e);
                            false
                        }
                    }
                }
                Err(e) => {
                    error!("write() failed: {:?} {:?}", peer.ip_port, e);
                    false
                }
            }
        }
        Err(e) => {
            error!("connect() failed: {:?} {:?}", peer.ip_port, e);
            false
        }
    }
}

fn torrent_loop(rx: Receiver<Command>, torrent_ref: Arc<Mutex<Torrent>>) {
    debug!("initiating torrent_loop for {:?}",
           torrent_ref.lock().unwrap().root_name);
    loop {
        let message = rx.recv().unwrap();
        debug!("{:?}", message);
        use self::Command::*;
        match message {
            StartDownload => {
                info!("starting download");

                // Handshake sequence, eliminates unhelpful peers
                {
                    let torrent = torrent_ref.lock().unwrap();
                    let info_hash = torrent.metainfo.info_hash();
                    let ih_ref = info_hash.as_ref();
                    let peer_id = &torrent.peer_id;
                    {
                        let mut trk_info = torrent.tracker_info.borrow_mut();
                        let mut active_peers = Vec::new();
                        let peer_count = trk_info.peers.len();

                        for _ in 0..peer_count {
                            let peer = trk_info.peers.pop().unwrap();
                            if handshake_peer(&peer, ih_ref, peer_id) {
                                active_peers.push(peer);
                            }
                        }
                        debug!("active peer count: {}", active_peers.len());
                        trk_info.peers = active_peers;
                    }
                }
            }
            _ => warn!("NOT YET IMPLEMENTED"),
        }
    }
}
