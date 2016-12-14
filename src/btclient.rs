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

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::net::SocketAddrV4;
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(dead_code)]
#[derive(Debug)]
pub struct BTClient {
    torrents: HashMap<usize, RefCell<Torrent>>,
    peer_id: String, // peer_id or client id
    port: u16, // between 6881-6889
    next_id: usize,
    channels: HashMap<usize, Sender<Message>>,
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
        // let (tx, rx): (Sender<Message>, Receiver<Message>) = mpsc::channel(1);
        let torrent = RefCell::new(Torrent::new(file));
        // let t_clone = torrent.clone();
        self.torrents.insert(self.next_id, torrent);
        // self.channels.insert(self.next_id, tx);
        self.next_id += 1;

        // let peer_id = self.id.clone();

        // thread::spawn(move || torrent_loop(rx, t_clone, peer_id));
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
                // let torrent = &(*(torrent.read().unwrap()));
                let root_name: String;
                if let Some(dir) = torrent.borrow()
                    .metainfo
                    .info()
                    .directory() {
                    root_name = dir.to_owned();
                } else {
                    root_name = torrent.borrow()
                        .metainfo
                        .info()
                        .files()
                        .next()
                        .unwrap()
                        .paths()
                        .next()
                        .unwrap()
                        .to_owned();
                }
                (*id, root_name)
            })
            .collect()
    }

    pub fn start_download(self: &BTClient, id: usize) {
        let torrent = &self.torrents[&id];
        let tracker_info = torrent.borrow().contact_tracker(self.peer_id.clone(), self.port);
        debug!("tracker_info: {:?}", tracker_info);
        (*self.torrents[&id].borrow_mut()).tracker_info = Some(tracker_info);
    }
}

#[derive(Debug)]
struct Torrent {
    metainfo: MetainfoFile,
    piece_bitmap: BitVec,

    tracker_info: Option<TrackerInfo>,
}

// From/For tracker
#[derive(Debug)]
struct TrackerInfo {
    peers: Vec<Peer>,
    uploaded: usize,
    downloaded: usize,
    left: usize,
    interval: usize, // in seconds
    tracker_id: Option<String>,
    num_seeders: usize,
    num_leachers: usize,
}

#[allow(dead_code)]
enum Message {
    StartDownload,
    StartSeed,
    StopDownload,
    StopSeed,
    Exit,
}

impl Torrent {
    fn new(mut file: fs::File) -> Torrent {
        // byte vector for metainfo storage
        let mut bytes: Vec<u8> = Vec::new();
        file.read_to_end(&mut bytes).unwrap();

        let metainfo = MetainfoFile::from_bytes(bytes).unwrap();
        // TODO consider making this optional
        print_metainfo_overview(&metainfo);

        let piece_count = metainfo.info().pieces().count();
        Torrent {
            metainfo: metainfo,
            piece_bitmap: BitVec::from_elem(piece_count, false),

            tracker_info: None,
        }
    }

    fn contact_tracker(self: &Torrent, peer_id: String, port: u16) -> TrackerInfo {
        let metainfo = &self.metainfo;
        let info_hash = metainfo.info_hash();
        let info_hash_str = unsafe { ::std::str::from_utf8_unchecked(info_hash.as_ref()) };

        // TODO parametrize these
        let uploaded = 0;
        let downloaded = 0;
        // TODO this needs to be calculated based on what we have
        let left_bytes = metainfo.info().files().fold(0, |acc, nex| acc + nex.length());

        let mut request_url = url::Url::parse(metainfo.main_tracker().unwrap()).unwrap();
        request_url.query_pairs_mut()
            .append_pair("info_hash", info_hash_str)
            .append_pair("peer_id", &peer_id)
            .append_pair("port", &port.to_string())
            .append_pair("uploaded", &uploaded.to_string())
            .append_pair("downloaded", &downloaded.to_string())
            .append_pair("left", &left_bytes.to_string())
            .append_pair("compact", "1")
            .append_pair("event", "started")
            .append_pair("supportcrypto", "0");
        trace!("Request URL {:?}", request_url);

        let client = hyper::client::Client::new();
        let mut http_resp =
            client.get(request_url).header(hyper::header::Connection::close()).send().unwrap();
        debug!("{:?}", http_resp);

        let mut buffer = Vec::new();
        http_resp.read_to_end(&mut buffer).unwrap();
        let response = bip_bencode::Bencode::decode(&buffer).unwrap();
        debug!("{:?}", response);

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
        debug!("interval: {:?}", interval);
        let interval: usize = 100;

        TrackerInfo {
            peers: peers,
            uploaded: uploaded,
            downloaded: uploaded,
            left: left_bytes as usize,
            interval: interval,
            tracker_id: None,
            num_seeders: 0,
            num_leachers: 0,
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
