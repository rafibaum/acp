//! Types for handling incoming file transfers.

use crate::minmax::Minmax;
use crate::mpsc as resizable;
use crate::proto::{packet, AckEndRound, EndTransfer, OutgoingData, Packet, SendPiece};
use crate::{proto, Terminated};
use bitvec::vec::BitVec;
use ring::digest;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Instant;

const INITIAL_AVG_PIECE_TIME: Duration = Duration::from_millis(1);
const PIECE_WINDOW: Duration = Duration::from_secs(10);
const GC_INTERVAL: u64 = 2;

/// Type for managing information relating to an incoming file transfer.
pub struct Incoming {
    id: Vec<u8>,
    file: BufWriter<File>,
    last_pos: u64,
    file_size: u64,
    blocks: HashMap<u64, BlockInfo>,
    /// Number of blocks in this file
    num_blocks: u64,
    /// Size of each block in pieces
    block_size: u32,
    /// Size of each piece in bytes
    piece_size: u32,
    info_rx: Receiver<IncomingPacket>,
    piece_rx: resizable::Receiver<SendPiece>,
    tx: Sender<OutgoingData>,
    blocks_received: u64,
    highest_piece: u64,
    pieces_received: BitVec,
    piece_relief: u32,
    marked_to: u64,
    marked: HashSet<u64>,
    lost: HashSet<u64>,
    next_gc: Option<Instant>,
    draining_round: Option<u32>,
    piece_start: Option<Instant>,
    piece_min: Duration,
    piece_filter: Minmax<Instant, Duration>,
    rtt: Arc<AtomicU64>,
    term_tx: Sender<Terminated>,
    window_size: u64,
    integrity: bool,
}

struct BlockInfo {
    num: u64,
    /// Number of pieces received
    received: u64,
    /// Total number of pieces
    pieces: u64,
    checksum: Option<Vec<u8>>,
    bytes: u64,
}

impl Incoming {
    /// Constructs a new incoming file transfer.
    pub fn new(
        id: Vec<u8>,
        info_rx: Receiver<IncomingPacket>,
        piece_rx: resizable::Receiver<SendPiece>,
        tx: Sender<OutgoingData>,
        file: File,
        file_size: u64,
        block_size: u32,
        piece_size: u32,
        rtt: Arc<AtomicU64>,
        term_tx: Sender<Terminated>,
        integrity: bool,
    ) -> Self {
        let blocks_cap = (file_size / piece_size as u64 + 1) / block_size as u64 + 1;
        let pieces_cap = file_size / piece_size as u64 + 1;

        let mut pieces_received = BitVec::with_capacity(pieces_cap as usize);
        pieces_received.resize(pieces_cap as usize, false);

        Incoming {
            id,
            file: BufWriter::new(file),
            last_pos: 0,
            file_size,
            blocks: HashMap::with_capacity(blocks_cap as usize),
            num_blocks: blocks_cap,
            block_size,
            piece_size,
            info_rx,
            piece_rx,
            tx,
            blocks_received: 0,
            highest_piece: 0,
            pieces_received,
            piece_relief: 0,
            marked_to: 0,
            marked: HashSet::new(),
            lost: HashSet::new(),
            next_gc: None,
            draining_round: None,
            piece_start: None,
            piece_min: INITIAL_AVG_PIECE_TIME,
            piece_filter: Minmax::new(Instant::now(), INITIAL_AVG_PIECE_TIME),
            rtt,
            term_tx,
            window_size: crate::INITIAL_WINDOW_SIZE,
            integrity,
        }
    }

    /// Starts an incoming file transfer.
    pub async fn run(mut self) {
        loop {
            // Splitting borrows
            let info_rx = &mut self.info_rx;
            let next_gc = self.next_gc.as_ref();
            let piece_rx = &mut self.piece_rx;
            let is_draining = self.draining_round.is_some();

            tokio::select! {
                biased;

                // GC triggered, prioritise when not draining
                true = async {
                    match next_gc {
                        Some(deadline) => {
                            tokio::time::sleep_until(*deadline).await;
                            true
                        }

                        None => false,
                    }
                }, if !is_draining => {
                    self.next_gc = None;
                    self.gc().await;
                }

                // Priority data
                packet = info_rx.recv() => {
                    let packet = packet.unwrap();
                    match packet {
                        IncomingPacket::BlockInfo(info) => {
                            if self.read_packet(IncomingData::BlockInfo(info)).await {
                                break;
                            }
                        }

                        IncomingPacket::EndRound(end) => {
                            println!("Ending round {}", end.round);
                            let gc_interval = Duration::from_nanos(self.rtt.load(Ordering::Relaxed) / GC_INTERVAL);
                            self.next_gc = Some(Instant::now() + gc_interval);
                            self.draining_round = Some(end.round);
                        }
                    }
                }

                // Incoming piece
                packet = piece_rx.recv() => {
                    let packet = packet.unwrap();
                    if self.read_packet(IncomingData::Piece(packet)).await {
                        break;
                    }
                }

                // GC triggered, drained queues
                true = async {
                    match next_gc {
                        Some(deadline) => {
                            tokio::time::sleep_until(*deadline).await;
                            true
                        }

                        None => false,
                    }
                }, if is_draining => {
                    self.next_gc = None;
                    self.gc().await;
                }
            }
        }

        self.tx
            .send(OutgoingData::Stream(Packet::new(
                packet::Data::EndTransfer(EndTransfer {
                    id: self.id.clone(),
                }),
            )))
            .await
            .unwrap();

        self.term_tx
            .send(Terminated::Incoming(self.id))
            .await
            .unwrap();
    }

    fn schedule_gc(&mut self) {
        if self.next_gc.is_some() {
            return;
        }

        let gc_interval = Duration::from_nanos(self.rtt.load(Ordering::Relaxed) / GC_INTERVAL);
        self.next_gc = Some(Instant::now() + gc_interval);
    }

    async fn read_packet(&mut self, packet: IncomingData) -> bool {
        self.schedule_gc();

        let block_num = match &packet {
            IncomingData::BlockInfo(info) => info.block as u64,
            IncomingData::Piece(piece) => {
                self.piece_start = Some(Instant::now());
                piece.piece / self.block_size as u64
            }
        };

        // Get block or insert empty block information
        //TODO: Handle OOB pieces
        let block = block(
            &mut self.blocks,
            block_num,
            self.file_size,
            self.num_blocks,
            self.block_size as u64,
            self.piece_size as u64,
        );

        match packet {
            // Attach block information as it's received
            IncomingData::BlockInfo(info) => {
                block.attach_info(info.checksum);
            }

            // Process incoming piece
            IncomingData::Piece(piece) => {
                // TODO: Piece bounds check
                *self.pieces_received.get_mut(piece.piece as usize).unwrap() = true;
                self.highest_piece = std::cmp::max(self.highest_piece, piece.piece);

                if !self.lost.contains(&piece.piece) {
                    // Lost packets already count towards window relief, so don't double count
                    self.piece_relief += 1;
                }

                let offset = piece.piece * self.piece_size as u64;
                if piece.piece % 1000 == 0 {
                    println!("Received piece {}", piece.piece);
                }

                if self.last_pos != offset {
                    self.file.seek(SeekFrom::Start(offset)).await.unwrap();
                    self.last_pos = offset;
                }

                block.write_piece(&mut self.file, &piece.data).await;
                self.last_pos += piece.data.len() as u64;

                let elapsed = Instant::now() - self.piece_start.unwrap();
                self.piece_min =
                    self.piece_filter
                        .running_min(PIECE_WINDOW, Instant::now(), elapsed);
            }
        };

        if block.has_all_pieces() && (block.has_checksum() || !self.integrity) {
            // Block received
            self.blocks_received += 1;
            println!(
                "Received block {}, done {} of {}",
                block.num, self.blocks_received, self.num_blocks
            );

            if self.blocks_received >= self.num_blocks {
                return if self.integrity {
                    self.verify_all_blocks().await
                } else {
                    true
                };
            }
        }

        false
    }

    async fn verify_all_blocks(&mut self) -> bool {
        let block_size = (self.block_size * self.piece_size) as u64;

        self.file.seek(SeekFrom::Start(0)).await.unwrap();

        for i in 0..self.num_blocks {
            let block = block(
                &mut self.blocks,
                i,
                self.file_size,
                self.num_blocks,
                self.block_size as u64,
                self.piece_size as u64,
            );

            let verified = block.verify(&mut self.file).await;
            self.last_pos += block_size;

            if !verified {
                panic!("Block {} did not match checksum", i);
            } else {
                println!("Verified block {}", i);
            }
        }

        true
    }

    async fn gc(&mut self) {
        let mut newly_lost = Vec::with_capacity(self.marked.len());
        for marked in &self.marked {
            if !self.pieces_received[*marked as usize] {
                // Piece has not been received, mark as lost
                newly_lost.push(*marked);
            }
        }
        self.marked.clear();

        if !newly_lost.is_empty() {
            self.lost.extend(newly_lost.iter());
        }

        let rtt = self.rtt.load(Ordering::Relaxed);
        let mut window_size = rtt / self.piece_min.as_nanos() as u64;
        // Build in a queue of double and assume minimum piece time is half of regular time
        window_size *= 4;
        self.window_size = std::cmp::max(self.window_size, window_size);

        self.piece_rx.resize(self.window_size as usize);
        // TODO: Handle window resizing differently

        let update = proto::ControlUpdate {
            id: self.id.clone(),
            received: self.piece_relief,
            lost: newly_lost,
            window_size,
        };

        self.tx
            .send(OutgoingData::Stream(Packet::new(
                packet::Data::ControlUpdate(update),
            )))
            .await
            .unwrap();

        self.piece_relief = 0;

        let range = match self.draining_round {
            Some(_) => &self.pieces_received[(self.marked_to as usize)..],
            None => &self.pieces_received[(self.marked_to as usize)..(self.highest_piece as usize)],
        };

        for (piece, received) in range.iter().enumerate() {
            if !received {
                self.marked.insert(self.marked_to + (piece as u64));
            }
        }

        self.marked_to = self.highest_piece;

        if !self.marked.is_empty() {
            self.schedule_gc();
        } else if let Some(round) = self.draining_round {
            self.end_round(round).await;
        }
    }

    async fn end_round(&mut self, round: u32) {
        println!("Starting next round, lost: {}", self.lost.len());

        self.highest_piece = 0;
        self.marked_to = 0;
        self.marked.clear();
        self.lost.clear();
        self.draining_round = None;

        let ack = Packet::new(packet::Data::AckEndRound(AckEndRound {
            id: self.id.clone(),
            round,
        }));

        self.tx.send(OutgoingData::Stream(ack)).await.unwrap();
    }
}

impl BlockInfo {
    fn new(num: u64, pieces: u64, bytes: u64) -> Self {
        BlockInfo {
            num,
            pieces,
            bytes,
            received: 0,
            checksum: None,
        }
    }

    async fn write_piece(&mut self, file: &mut BufWriter<File>, data: &[u8]) {
        self.received += 1;
        file.write_all(&data).await.unwrap();
    }

    fn has_all_pieces(&self) -> bool {
        self.received >= self.pieces
    }

    fn attach_info(&mut self, digest: Vec<u8>) {
        self.checksum = Some(digest);
    }

    fn has_checksum(&self) -> bool {
        self.checksum.is_some()
    }

    async fn verify(&mut self, file: &mut BufWriter<File>) -> bool {
        let checksum = match &self.checksum {
            Some(checksum) => checksum,
            None => return false,
        };

        let mut ctx = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0; 65535];

        let mut read = 0;
        loop {
            let size = std::cmp::min(buf.len(), (self.bytes - read) as usize);
            let len = file.read(&mut buf[..size]).await.unwrap();

            //TODO: Validate piece size
            if len == 0 {
                break;
            }

            read += len as u64;
            ctx.update(&buf[..len]);

            if read >= self.bytes {
                break;
            }
        }

        let value = ctx.finish();

        value.as_ref() == checksum
    }
}

/// A packet routed into an incoming transfer.
#[derive(Debug)]
pub enum IncomingPacket {
    /// Incoming information for a block.
    BlockInfo(proto::BlockInfo),
    /// Informing the receiver the round has ended so it can start draining the transfer.
    EndRound(proto::EndRound),
}

/// Network traffic routed into an inbound transfer.
#[derive(Debug)]
pub enum IncomingData {
    /// An incoming piece.
    Piece(proto::SendPiece),
    /// Incoming information for a block.
    BlockInfo(proto::BlockInfo),
}

#[inline]
fn block(
    blocks: &mut HashMap<u64, BlockInfo>,
    block: u64,
    file_size: u64,
    num_blocks: u64,
    block_size: u64,
    piece_size: u64,
) -> &mut BlockInfo {
    blocks.entry(block).or_insert_with(|| {
        let (pieces, bytes) = if block == num_blocks - 1 {
            // There's a different number of pieces in the last block
            let block_bytes = block_size * piece_size;
            let prec_bytes = (num_blocks - 1) * block_bytes;
            let rem_bytes = file_size - prec_bytes;
            let rem_pieces = rem_bytes / piece_size as u64 + 1;

            (rem_pieces, rem_bytes)
        } else {
            (block_size as u64, block_size * piece_size)
        };

        BlockInfo::new(block, pieces, bytes)
    })
}
