//! Types for handling incoming file transfers.

use crate::proto;
use crate::proto::AckEndRound;
use bitvec::vec::BitVec;
use ring::digest;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

/// Type for managing information relating to an incoming file transfer.
pub struct Incoming {
    id: Vec<u8>,
    file: File,
    file_size: u64,
    blocks: HashMap<u64, BlockInfo>,
    /// Number of blocks in this file
    num_blocks: u64,
    /// Size of each block in pieces
    block_size: u32,
    /// Size of each piece in bytes
    piece_size: u32,
    //TODO: Add reasonable buffer sizes
    rx: UnboundedReceiver<IncomingPacket>,
    tx: UnboundedSender<OutgoingPacket>,
    blocks_received: u64,
    highest_piece: u64,
    pieces_received: BitVec,
    piece_relief: u32,
    marked_to: u64,
    marked: HashSet<u64>,
    lost: HashSet<u64>,
    gc_interval: Duration,
    next_gc: Option<Instant>,
    draining_round: Option<DrainState>,
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
        rx: UnboundedReceiver<IncomingPacket>,
        tx: UnboundedSender<OutgoingPacket>,
        file: File,
        file_size: u64,
        block_size: u32,
        piece_size: u32,
        gc_interval: Duration,
    ) -> Self {
        let blocks_cap = (file_size / piece_size as u64 + 1) / block_size as u64 + 1;
        let pieces_cap = file_size / piece_size as u64 + 1;

        let mut pieces_received = BitVec::with_capacity(pieces_cap as usize);
        pieces_received.resize(pieces_cap as usize, false);

        Incoming {
            id,
            file,
            file_size,
            blocks: HashMap::with_capacity(blocks_cap as usize),
            num_blocks: blocks_cap,
            block_size,
            piece_size,
            rx,
            tx,
            blocks_received: 0,
            highest_piece: 0,
            pieces_received,
            piece_relief: 0,
            marked_to: 0,
            marked: HashSet::new(),
            lost: HashSet::new(),
            gc_interval,
            next_gc: None,
            draining_round: None,
        }
    }

    /// Starts an incoming file transfer.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                // Receive a packet
                packet = self.rx.recv() => {
                    match packet {
                        Some(packet) => {
                            match packet {
                                IncomingPacket::Data(packet) => {
                                    if self.read_packet(packet).await {
                                        break;
                                    }
                                }

                                IncomingPacket::EndRound(end) => {
                                    self.next_gc = Some(Instant::now() + self.gc_interval);
                                    self.draining_round = Some(DrainState::Draining(end.round));
                                }
                            }

                        },
                        None => panic!("File transfer channel dropped"),
                    }
                }

                // GC triggered
                true = Self::next_gc(self.next_gc.as_ref()) => {
                    self.next_gc = None;
                    self.gc();
                }
            }
        }
    }

    async fn next_gc(deadline: Option<&Instant>) -> bool {
        match deadline {
            Some(deadline) => {
                tokio::time::sleep_until(*deadline).await;
                true
            }

            None => false,
        }
    }

    fn schedule_gc(&mut self) {
        if self.next_gc.is_some() {
            return;
        }

        self.next_gc = Some(Instant::now() + self.gc_interval);
    }

    async fn read_packet(&mut self, packet: IncomingData) -> bool {
        self.schedule_gc();

        let block_num = match &packet {
            IncomingData::BlockInfo(info) => info.block as u64,
            IncomingData::Piece(piece) => piece.piece / self.block_size as u64,
        };

        // Get block or insert empty block information
        //TODO: Handle OOB pieces
        //TODO: Better closure capture semantics, move inline
        let block = block(
            &mut self.blocks,
            block_num,
            self.file_size,
            self.num_blocks,
            self.block_size as u64,
            self.piece_size as u64,
        );

        let result = match packet {
            // Attach block information as it's received
            IncomingData::BlockInfo(info) => block.attach_info(info.checksum),

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
                println!("Received piece {}", piece.piece);
                block.write_piece(&mut self.file, offset, &piece.data).await
            }
        };

        // Check if we now have a complete block
        if let BlockStatus::Done = result {
            if block
                .verify(
                    &mut self.file,
                    block_num * (self.block_size * self.piece_size) as u64,
                )
                .await
            {
                self.blocks_received += 1;
                println!(
                    "Verified block {}, done {} of {}",
                    block.num, self.blocks_received, self.num_blocks
                );

                if self.blocks_received >= self.num_blocks {
                    return true;
                }
            } else {
                //TODO: Proper checksum mismatch handling
                panic!("Does not match");
            }
        }

        false
    }

    fn gc(&mut self) {
        let mut newly_lost = Vec::with_capacity(self.marked.len());
        for marked in &self.marked {
            if !self.pieces_received[*marked as usize] {
                // Piece has not been received, mark as lost
                newly_lost.push(*marked);
            } else {
            }
        }
        self.marked.clear();

        if !newly_lost.is_empty() {
            self.lost.extend(newly_lost.iter());
        }

        let update = proto::ControlUpdate {
            id: self.id.clone(),
            received: self.piece_relief,
            lost: newly_lost,
        };

        println!(
            "Update, received: {}, lost: {}",
            update.received,
            self.lost.len()
        );

        self.tx.send(OutgoingPacket::ControlUpdate(update)).unwrap();

        self.piece_relief = 0;

        let range = match self.draining_round {
            Some(DrainState::Drained(round)) => {
                self.end_round(round);
                return;
            }
            Some(DrainState::Draining(round)) => {
                self.draining_round = Some(DrainState::Drained(round));
                &self.pieces_received[(self.marked_to as usize)..]
            }
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
        } else if let Some(draining) = self.draining_round {
            self.end_round(draining.round());
        }
    }

    fn end_round(&mut self, round: u32) {
        self.highest_piece = 0;
        self.marked_to = 0;
        self.marked.clear();
        self.lost.clear();
        self.draining_round = None;

        self.tx
            .send(OutgoingPacket::AckEndRound(AckEndRound { round }))
            .unwrap();
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

    async fn write_piece(&mut self, file: &mut File, offset: u64, data: &[u8]) -> BlockStatus {
        self.received += 1;

        file.seek(SeekFrom::Start(offset)).await.unwrap();
        file.write_all(&data).await.unwrap();

        self.check_done()
    }

    fn attach_info(&mut self, digest: Vec<u8>) -> BlockStatus {
        self.checksum = Some(digest);
        self.check_done()
    }

    fn check_done(&self) -> BlockStatus {
        match &self.checksum {
            None => BlockStatus::Pending,
            Some(_) => {
                if self.received >= self.pieces {
                    BlockStatus::Done
                } else {
                    BlockStatus::Pending
                }
            }
        }
    }

    async fn verify(&mut self, file: &mut File, offset: u64) -> bool {
        let checksum = match &self.checksum {
            Some(checksum) => checksum,
            None => return false,
        };

        let mut ctx = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0; 65535];
        file.seek(SeekFrom::Start(offset)).await.unwrap();

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

/// Completion status of a block.
enum BlockStatus {
    /// Block is missing some information or pieces.
    Pending,
    /// Block is complete.
    Done,
}

/// A packet routed into an incoming transfer.
#[derive(Debug)]
pub enum IncomingPacket {
    Data(IncomingData),
    EndRound(proto::EndRound),
}

#[derive(Debug)]
pub enum IncomingData {
    /// An incoming piece.
    Piece(proto::SendPiece),
    /// Incoming information for a block.
    BlockInfo(proto::BlockInfo),
}

#[derive(Debug)]
pub enum OutgoingPacket {
    ControlUpdate(proto::ControlUpdate),
    AckEndRound(proto::AckEndRound),
}

#[derive(Copy, Clone)]
enum DrainState {
    Draining(u32),
    Drained(u32),
}

impl DrainState {
    fn round(&self) -> u32 {
        *match self {
            DrainState::Draining(num) => num,
            DrainState::Drained(num) => num,
        }
    }
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
