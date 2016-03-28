extern crate rollsum;
extern crate crypto;
#[macro_use]
extern crate log;
extern crate rustc_serialize as serialize;

use std::io::{Read, Write};
use std::{fs, mem, thread, io};
use std::path::{Path, PathBuf};
use serialize::hex::ToHex;

use std::sync::mpsc;

use rollsum::Engine;
use crypto::sha2;
use crypto::digest::Digest;

#[derive(Copy, Clone, Debug)]
enum ChunkType {
    Index,
    Data,
}

enum Message {
    Data(Vec<u8>, Vec<Edge>, ChunkType),
    Exit,
}

/// Edge: offset in the input and sha256 sum of the chunk
type Edge = (usize, Vec<u8>);

struct Chunker {
    roll : rollsum::Bup,
    sha256 : sha2::Sha256,
    bytes_total : usize,
    bytes_chunk: usize,
    chunks_total : usize,

    edges : Vec<Edge>,
}

impl Chunker {
    pub fn new() -> Self {
        Chunker {
            roll: rollsum::Bup::new(),
            sha256: sha2::Sha256::new(),
            bytes_total: 0,
            bytes_chunk: 0,
            chunks_total: 0,
            edges: vec!(),
        }
    }

    pub fn edge_found(&mut self, input_ofs : usize) {
        debug!("found edge at {}; sum: {:x}",
                 self.bytes_total,
                 self.roll.digest());

        debug!("sha256 hash: {}",
                 self.sha256.result_str());

        let mut sha256 = vec![0u8; 32];
        self.sha256.result(&mut sha256);

        self.edges.push((input_ofs, sha256));

        self.chunks_total += 1;
        self.bytes_chunk += 0;

        self.sha256.reset();
        self.roll = rollsum::Bup::new();
    }

    pub fn input(&mut self, buf : &[u8]) -> Vec<Edge> {
        let mut ofs : usize = 0;
        let len = buf.len();
        while ofs < len {
            if let Some(count) = self.roll.find_chunk_edge(&buf[ofs..len]) {
                self.sha256.input(&buf[ofs..ofs+count]);

                ofs += count;

                self.bytes_chunk += count;
                self.bytes_total += count;
                self.edge_found(ofs);
            } else {
                let count = len - ofs;
                self.sha256.input(&buf[ofs..len]);
                self.bytes_chunk += count;
                self.bytes_total += count;
                break
            }
        }
        mem::replace(&mut self.edges, vec!())
    }

    pub fn finish(&mut self) -> Vec<Edge> {
        if self.bytes_chunk != 0 {
            self.edge_found(0);
        }
        mem::replace(&mut self.edges, vec!())
    }
}

/// Store data, using input_f to get chunks of data
///
/// Return final digest
fn store_with_io_func(tx : mpsc::Sender<Message>,
                      mut input_f : &mut FnMut() -> Vec<u8>,
                      chunk_type : ChunkType,
                      ) -> Vec<u8> {
    let mut chunker = Chunker::new();

    let mut index : Vec<u8> = vec!();
    loop {
        let buf = input_f();
        let len = buf.len();

        if len == 0 {
            break;
        }

        let edges = chunker.input(&buf[..len]);

        for &(_, ref sum) in &edges {
            index.append(&mut sum.clone());
        }
        tx.send(Message::Data(buf, edges, chunk_type)).unwrap();
    }
    let edges = chunker.finish();

    for &(_, ref sum) in &edges {
        index.append(&mut sum.clone());
    }
    tx.send(Message::Data(vec!(), edges, chunk_type)).unwrap();

    println!("index size: {}", index.len());
    println!("chunks found: {}", chunker.chunks_total);

    if index.len() > 32 {
        store_with_io_func(tx, &mut || mem::replace(&mut index, vec!()), ChunkType::Index)
    } else {
        index
    }

}

/// Store stdio and return a digest
fn store_stdio(tx : mpsc::Sender<Message>) -> Vec<u8> {
    let mut stdin = io::stdin();
    store_with_io_func(tx, &mut || {
        let mut buf = vec![0u8; 16 * 1024];
        let len = stdin.read(&mut buf).unwrap();
        buf.truncate(len);
        buf
    }, ChunkType::Data)
}

fn digest_to_path(chunk_type : ChunkType, digest : &[u8]) -> PathBuf {
    match chunk_type {
        ChunkType::Data => Path::new("chunks"),
        ChunkType::Index => Path::new("index"),
    }.join(&digest[0..1].to_hex()).join(digest[1..2].to_hex()).join(&digest.to_hex())
}

fn chunk_writer(dest_base_dir : &Path, rx : mpsc::Receiver<Message>) {
    let mut pending_data = vec!();
    loop {
        match rx.recv().unwrap() {
            Message::Exit => {
                assert!(pending_data.is_empty());
                return
            }
            Message::Data(data, edges, chunk_type) => if edges.is_empty() {
                println!("0 edges");
                pending_data.push(data)
            } else {
                println!("{} edges", edges.len());
                let mut prev_ofs = 0;
                for &(ref ofs, ref sha256) in &edges {
                    let path = digest_to_path(chunk_type, &sha256);
                    let path = dest_base_dir.join(path);
                    println!("Would write {:?}", path);
                    if !path.exists() {
                        fs::create_dir_all(path.parent().unwrap()).unwrap();
                        let mut chunk_file = fs::File::create(path).unwrap();

                        for data in pending_data.drain(..) {
                            chunk_file.write(&data).unwrap();
                        }

                        if *ofs != prev_ofs {
                            chunk_file.write(&data[prev_ofs..*ofs]).unwrap();
                        }
                    } else {
                        pending_data.clear();
                    }
                    debug_assert!(pending_data.is_empty());

                    prev_ofs = *ofs;
                }
                if prev_ofs != data.len() {
                    let mut data = data;
                    pending_data.push(data.split_off(prev_ofs))
                }
            }
        }
    }
}

fn main() {
    let (tx, rx) = mpsc::channel();

    let chunk_writer_join = thread::spawn(|| chunk_writer(Path::new("backup_dir"), rx));

    store_stdio(tx.clone());

    tx.send(Message::Exit).unwrap();
    chunk_writer_join.join().unwrap();
}
