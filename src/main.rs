use std::{
    collections::BTreeMap,
    env,
    fs::File,
    io::Read,
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
};

use bytes::{BufMut, BytesMut};
use serde::{Serialize, Deserialize};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};

// Available if you need it!
use serde_bencode;

#[derive(Serialize, Deserialize)]
struct InfoDict {
    name: String,
    #[serde(rename = "piece length")]
    piece_length: u64,
    pieces: ByteBuf,
    length: u64,
}

#[derive(Serialize, Deserialize)]
struct Metainfo {
    announce: String,
    info: InfoDict,
}

#[derive(Clone)]
enum Bencoded {
    String(Vec<u8>),
    Integer(i64),
    List(Vec<Bencoded>),
    Dict(BTreeMap<String, Bencoded>),
}

impl Bencoded {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::String(v) => serde_json::Value::String(
                String::from_utf8(v.to_vec())
                    .expect("bencoded string can only convert to json if utf-8"),
            ),
            Self::Integer(i) => serde_json::Value::Number((*i).into()),
            Self::List(l) => serde_json::Value::Array(l.iter().map(|e| e.to_json()).collect()),
            Self::Dict(d) => serde_json::Value::Object(serde_json::Map::from_iter(
                d.iter().map(|(k, v)| (k.clone(), v.to_json())),
            )),
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(512);
        match self {
            Self::String(v) => {
                buf.put_slice(v.len().to_string().as_bytes());
                buf.put_u8(b':');
                buf.put_slice(v);
            }
            Self::Integer(i) => {
                buf.put_u8(b'i');
                buf.put_slice(i.to_string().as_bytes());
                buf.put_u8(b'e');
            }
            Self::List(l) => {
                buf.put_u8(b'l');
                buf.put_slice(&l.iter().flat_map(|b| b.serialize()).collect::<Vec<u8>>());
                buf.put_u8(b'e');
            }
            Self::Dict(d) => {
                buf.put_u8(b'd');
                buf.put_slice(
                    &d.iter()
                        .flat_map(|(k, v)| {
                            let mut b = k.len().to_string().as_bytes().to_vec();
                            b.push(b':');
                            b.append(&mut k.as_bytes().to_vec());
                            b.append(&mut v.serialize());
                            b
                        })
                        .collect::<Vec<u8>>(),
                );
                buf.put_u8(b'e');
            }
        }
        buf.to_vec()
    }
}

fn decode_bencoded_value(encoded_value: &[u8]) -> (Bencoded, &[u8]) {
    // If encoded_value starts with a digit, it's a string
    let mut chars = encoded_value.iter().peekable();
    match chars.next() {
        Some(b'd') => {
            let mut dict = BTreeMap::new();
            let mut rest: Vec<u8>;
            while chars.peek() != Some(&&b'e') {
                rest = chars.copied().collect::<Vec<u8>>();
                let (key, r) = decode_bencoded_value(&rest);
                match key {
                    Bencoded::String(key) => {
                        let (val, r) = decode_bencoded_value(r);
                        dict.insert(String::from_utf8(key).expect("key is valid utf-8"), val);
                        chars = r.iter().peekable();
                    }
                    _ => panic!("bencoded dictionary keys must be strings"),
                }
            }
            chars.next();
            (
                Bencoded::Dict(dict),
                &encoded_value[encoded_value.len() - chars.count()..],
            )
        }
        Some(b'l') => {
            let mut vals = vec![];
            let mut rest: Vec<u8>;
            while chars.peek() != Some(&&b'e') {
                rest = chars.copied().collect::<Vec<u8>>();
                let (v, r) = decode_bencoded_value(&rest);
                vals.push(v);
                chars = r.iter().peekable();
            }
            chars.next();
            (
                Bencoded::List(vals),
                &encoded_value[encoded_value.len() - chars.count()..],
            )
        }
        Some(b'i') => {
            let numerals: Vec<u8> = chars
                .map_while(|c| if *c != b'e' { Some(*c) } else { None })
                .collect();
            let integer: i64 = String::from_utf8(numerals.clone())
                .expect("number must be valid utf-8")
                .parse()
                .expect("failed to parse numerals into integer");
            (
                Bencoded::Integer(integer),
                &encoded_value[numerals.len() + 2..],
            )
        }
        Some(c) if c.is_ascii_digit() => {
            // Example: "5:hello" -> "hello"
            let colon_index = {
                let mut i = 0;
                while encoded_value[i] != b':' {
                    i += 1;
                }
                i
            };
            let number_string = String::from_utf8(encoded_value[..colon_index].to_vec())
                .expect("number string must be valid utf-8");
            let number = number_string.parse::<usize>().unwrap();
            let bytes = &encoded_value[colon_index + 1..colon_index + 1 + number];
            (
                Bencoded::String(bytes.to_vec()),
                &encoded_value[colon_index + 1 + number..],
            )
        }
        Some(_) | None => {
            panic!(
                "Unhandled encoded value: {}",
                String::from_utf8_lossy(encoded_value)
            )
        }
    }
}

// Usage: your_bittorrent.sh decode "<encoded_value>"
fn main() {
    let args: Vec<String> = env::args().collect();
    let command = &args[1];

    match command.trim() {
        "decode" => {
            let encoded_value = &args[2]
                .chars()
                .map(|c| c.try_into().expect("utf-8"))
                .collect::<Vec<u8>>();
            let (decoded_value, rest) = decode_bencoded_value(encoded_value);
            println!("{}", decoded_value.to_json());
            eprintln!("rest: {}", String::from_utf8_lossy(rest));
        }
        "peers" => {
            let torrent_path = Path::new(&args[2]);
            //eprintln!("looking at torrent file: {}", torrent_path.display());
            let mut file = match File::open(torrent_path) {
                Err(why) => panic!("couldn't open {}: {}", torrent_path.display(), why),
                Ok(file) => file,
            };
            let fsz = file
                .metadata()
                .expect("couldn't read torrent file metadata")
                .len();
            //eprintln!("torrent file is {} bytes", fsz);
            let mut cts = Vec::with_capacity(
                fsz.try_into()
                    .expect("couldn't make a buffer big enough to hold entire torrent file"),
            );
            match file.read_to_end(&mut cts) {
                Ok(0) => panic!("nothing read from torrent file"),
                Ok(_bsz) => {} //eprintln!("read {} bytes into buffer", bsz),
                Err(why) => panic!(
                    "error reading torrent file {}: {}",
                    torrent_path.display(),
                    why
                ),
            }
            let (decoded_value, _rest) = decode_bencoded_value(&cts);
            let tracker: String;
            let length: i64;
            let info_dict: Bencoded;
            match &decoded_value {
                Bencoded::Dict(d) => {
                    assert!(
                        d.contains_key("announce"),
                        "torrent file dict must contain announce key"
                    );
                    assert!(
                        d.contains_key("info"),
                        "torrent file dict must contain info key"
                    );
                    match d.get("announce") {
                        Some(Bencoded::String(s)) => {
                            tracker = String::from_utf8(s.to_vec())
                                .expect("announce value must be valid utf-8")
                        }
                        _ => panic!("torrent file announce key's value is not a string"),
                    }
                    match d.get("info") {
                        Some(Bencoded::Dict(d)) => {
                            info_dict = Bencoded::Dict(d.clone());
                            match d.get("length") {
                                Some(Bencoded::Integer(i)) => length = *i,
                                _ => panic!("info.length is not an integer"),
                            }
                        }
                        _ => panic!("torrent file is missing info dict"),
                    }
                }
                _ => panic!("torrent file isn't a bencoded dict"),
            }
            let infodict_ser = info_dict.serialize();
            //eprintln!("infodict_ser: {}", String::from_utf8_lossy(&infodict_ser));
            let mut hasher = Sha1::new();
            hasher.update(infodict_ser);
            let infohash = hasher.finalize();
            let ih_urlenc = infohash
                .iter()
                .map(|b| match *b {
                    b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'.' | b'_' | b'~' => {
                        format!("{}", *b as char)
                    }
                    _ => format!("%{:02X}", b),
                })
                .collect::<String>();
            //eprintln!("ih_urlenc: {}", ih_urlenc);

            eprintln!("fetching peers from tracker at {}", tracker);
            let tracker_client = reqwest::blocking::Client::new();
            let mut req = tracker_client
                .get(tracker)
                .query(&[
                    ("peer_id", "00112233445566778899"),
                    ("left", &length.to_string()),
                    ("port", "6881"),
                    ("uploaded", "0"),
                    ("downloaded", "0"),
                    ("compact", "1"),
                ])
                .build()
                .expect("failed to create valid peers request");
            let q = req
                .url()
                .query()
                .expect("query parameters were not created");
            let newq = q.to_owned() + "&info_hash=" + &ih_urlenc;
            req.url_mut().set_query(Some(&newq));

            //eprintln!("request: {:?}", req);
            let mut res = tracker_client
                .execute(req)
                .expect("failed to get from tracker");
            let body = {
                let mut buf = vec![].writer();
                res.copy_to(&mut buf)
                    .expect("could not read response from tracker");
                buf.into_inner()
            };
            //eprintln!("got a response: {}", String::from_utf8_lossy(&body));
            let (announce, _rest) = decode_bencoded_value(&body);
            let peers: Vec<SocketAddrV4>;
            match announce {
                Bencoded::Dict(d) => {
                    if let Some(Bencoded::String(s)) = d.get("failure reason") {
                        panic!(
                            "tracker responded with an error: {}",
                            String::from_utf8_lossy(s)
                        )
                    }
                    match d.get("peers") {
                        Some(Bencoded::String(s)) => {
                            peers = s
                                .chunks(6)
                                .map(|peer| {
                                    let mut ipbytes: [u8; 4] = [0; 4];
                                    ipbytes.copy_from_slice(&peer[0..4]);
                                    let mut skbytes = [0u8; 2];
                                    skbytes.copy_from_slice(&peer[4..6]);
                                    SocketAddrV4::new(
                                        Ipv4Addr::from(ipbytes),
                                        u16::from_be_bytes(skbytes),
                                    )
                                })
                                .collect();
                        }
                        Some(_) => panic!("tracker response contains peers not encoded as string"),
                        None => panic!("tracker response does not contain peers key"),
                    }
                }
                _ => panic!("got non-dict response from tracker"),
            }
            for p in peers.iter() {
                println!("{}", p);
            }
        }
        "info" => {
            let torrent_path = Path::new(&args[2]);
            //eprintln!("looking at torrent file: {}", torrent_path.display());
            let mut file = match File::open(torrent_path) {
                Err(why) => panic!("couldn't open {}: {}", torrent_path.display(), why),
                Ok(file) => file,
            };
            let fsz = file
                .metadata()
                .expect("couldn't read torrent file metadata")
                .len();
            //eprintln!("torrent file is {} bytes", fsz);
            let mut cts = Vec::with_capacity(
                fsz.try_into()
                    .expect("couldn't make a buffer big enough to hold entire torrent file"),
            );
            match file.read_to_end(&mut cts) {
                Ok(0) => panic!("nothing read from torrent file"),
                Ok(_bsz) => {} //eprintln!("read {} bytes into buffer", bsz),
                Err(why) => panic!(
                    "error reading torrent file {}: {}",
                    torrent_path.display(),
                    why
                ),
            }
            let metainf: Metainfo = serde_bencode::from_bytes(&cts).expect("could not deserialize metainfo file");
            println!("Tracker URL: {}", metainf.announce);
            println!("Length: {}", metainf.info.length);
            let mut hasher = Sha1::new();
            hasher.update(serde_bencode::to_bytes(&metainf.info).expect("could not serialize info dict for hashing"));
            let infohash = hasher.finalize();
            println!("Info Hash: {}", hex::encode(infohash));
            println!("Piece Length: {}", metainf.info.piece_length);
            println!("Piece Hashes:");
            for ph in metainf.info.pieces.chunks(20).map(Vec::from) {
                println!("{}", hex::encode(ph));
            }
        }
        _ => {
            println!("unknown command: {}", args[1])
        }
    }
}
