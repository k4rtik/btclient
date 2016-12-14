extern crate bip_metainfo;
extern crate bit_vec;
extern crate chrono;
extern crate url;

use self::bip_metainfo::MetainfoFile;
use self::bit_vec::BitVec;
use self::chrono::TimeZone;

use std::collections::HashMap;
use std::io::Read;
use std::fs;
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(dead_code)]
pub struct BTClient {
    torrents: HashMap<usize, Torrent>,
    peer_id: String, // peer_id or client id
    next_id: usize,
    channels: HashMap<usize, Sender<Message>>,
}

impl BTClient {
    pub fn new() -> BTClient {
        let now = SystemTime::now();
        let duration = now.duration_since(UNIX_EPOCH).unwrap();
        let peer_id = "-bittorrent-rs-".to_owned() + &format!("{}", duration.as_secs() % 100_000);
        debug!("peer_id: {}", peer_id);
        BTClient {
            torrents: HashMap::new(),
            peer_id: peer_id,
            next_id: 0,
            channels: HashMap::new(),
        }
    }

    pub fn add(self: &mut BTClient, file: fs::File) -> Result<(), String> {
        // let (tx, rx): (Sender<Message>, Receiver<Message>) = mpsc::channel(1);
        let torrent = Torrent::new(file);
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
                if let Some(dir) = torrent.metainfo
                    .info()
                    .directory() {
                    root_name = dir.to_owned();
                } else {
                    root_name = torrent.metainfo
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

    pub fn get_id(self: &BTClient) -> String {
        self.peer_id.clone()
    }
}

pub struct Torrent {
    metainfo: MetainfoFile,

    // From/For tracker
    pub peers: Vec<Peer>,
    pub uploaded: usize,
    pub downloaded: usize,
    pub left: usize,
    pub interval: usize, // in seconds
    pub tracker_id: String,
    pub num_seeders: usize,
    pub num_leachers: usize,
    pub piece_bitmap: BitVec,
}

#[allow(dead_code)]
pub enum Message {
    StartDownload,
    StartSeed,
    StopDownload,
    StopSeed,
    Exit,
}

impl Torrent {
    pub fn new(mut file: fs::File) -> Torrent {
        // byte vector for metainfo storage
        let mut bytes: Vec<u8> = Vec::new();
        file.read_to_end(&mut bytes).unwrap();

        let metainfo = MetainfoFile::from_bytes(bytes).unwrap();
        // TODO consider making this optional
        print_metainfo_overview(&metainfo);

        let piece_count = metainfo.info().pieces().count();
        Torrent {
            metainfo: metainfo,

            peers: Vec::new(),
            uploaded: 0,
            downloaded: 0,
            left: 0,
            interval: 0, // in seconds
            tracker_id: String::new(),
            num_seeders: 0,
            num_leachers: 0,
            piece_bitmap: BitVec::from_elem(piece_count, false),
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
    println!("Main Tracker: {}", metainfo.main_tracker().unwrap_or("<missing>"));
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
pub struct Peer {
    id: String, // peer_id
    ip_port: String,

    am_choking: bool,
    peer_choking: bool,
    am_interested: bool,
    peer_interested: bool,
}

impl Peer {
    pub fn new(id: String, ip_port: String) -> Peer {
        Peer {
            id: id,
            ip_port: ip_port,
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
        }
    }
}

#[allow(dead_code)]
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
enum EventType {
    Started,
    Stopped,
    Completed,
}
