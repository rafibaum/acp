//! Types for handling incoming file transfers.

use crate::minmax::Minmax;
use crate::proto::{packet, AckEndRound, EndTransfer, OutgoingData, Packet};
use crate::{proto, Terminated};
use bitvec::vec::BitVec;
use ring::digest;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Instant;
use tracing::{instrument, trace};

const INITIAL_AVG_PIECE_TIME: Duration = Duration::from_millis(1);
const PIECE_WINDOW: Duration = Duration::from_secs(10);
const GC_INTERVAL: u64 = 2;

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
    rx: Receiver<IncomingPacket>,
    tx: Sender<OutgoingData>,
    blocks_received: u64,
    highest_piece: u64,
    pieces_received: BitVec,
    piece_relief: u32,
    marked_to: u64,
    marked: HashSet<u64>,
    lost: HashSet<u64>,
    next_gc: Option<Instant>,
    draining_round: Option<DrainState>,
    piece_start: Option<Instant>,
    piece_min: Duration,
    piece_filter: Minmax<Instant, Duration>,
    rtt: Arc<AtomicU64>,
    term_tx: Sender<Terminated>,
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
        rx: Receiver<IncomingPacket>,
        tx: Sender<OutgoingData>,
        file: File,
        file_size: u64,
        block_size: u32,
        piece_size: u32,
        rtt: Arc<AtomicU64>,
        term_tx: Sender<Terminated>,
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
            next_gc: None,
            draining_round: None,
            piece_start: None,
            piece_min: INITIAL_AVG_PIECE_TIME,
            piece_filter: Minmax::new(Instant::now(), INITIAL_AVG_PIECE_TIME),
            rtt,
            term_tx,
        }
    }

    /// Starts an incoming file transfer.
    #[instrument(name = "incoming", level = "trace", skip(self), fields(id = ?self.id))]
    pub async fn run(mut self) {
        loop {
            // Splitting borrows
            let rx = &mut self.rx;
            let next_gc = self.next_gc.as_ref();

            tokio::select! {
                // Receive a packet
                packet = rx.recv() => {
                    match packet {
                        Some(packet) => {
                            match packet {
                                IncomingPacket::Data(packet) => {
                                    if self.read_packet(packet).await {
                                        break;
                                    }
                                }

                                IncomingPacket::EndRound(end) => {
                                    trace!(round = end.round, "Ending round");
                                    let gc_interval = Duration::from_nanos(self.rtt.load(Ordering::Relaxed) / GC_INTERVAL);
                                    self.next_gc = Some(Instant::now() + gc_interval);
                                    self.draining_round = Some(DrainState::Draining(end.round));
                                }
                            }

                        },
                        None => panic!("File transfer channel dropped"),
                    }
                }

                // GC triggered
                true = async {
                    match next_gc {
                        Some(deadline) => {
                            tokio::time::sleep_until(*deadline).await;
                            true
                        }

                        None => false,
                    }
                } => {
                    self.next_gc = None;
                    self.gc().await;
                }
            }
        }

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

                trace!(piece = piece.piece, "Received piece");

                let offset = piece.piece * self.piece_size as u64;
                let block_status = block.write_piece(&mut self.file, offset, &piece.data).await;

                let elapsed = Instant::now() - self.piece_start.unwrap();
                self.piece_min =
                    self.piece_filter
                        .running_min(PIECE_WINDOW, Instant::now(), elapsed);

                block_status
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
                trace!(
                    block_num,
                    blocks_received = self.blocks_received,
                    total_blocks = self.num_blocks,
                    "Completed block"
                );

                if self.blocks_received >= self.num_blocks {
                    self.tx
                        .send(OutgoingData::Stream(Packet::new(
                            packet::Data::EndTransfer(EndTransfer {
                                id: self.id.clone(),
                            }),
                        )))
                        .await
                        .unwrap();
                    return true;
                }
            } else {
                //TODO: Proper checksum mismatch handling
                panic!("Does not match");
            }
        }

        false
    }

    async fn gc(&mut self) {
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

        let rtt = self.rtt.load(Ordering::Relaxed);
        let mut window_size = rtt / self.piece_min.as_nanos() as u64;
        // Build in a queue of double and assume minimum piece time is half of regular time
        window_size *= 4;

        trace!(
            received = self.piece_relief,
            lost = newly_lost.len(),
            target_window = window_size,
            rtt,
            piece_min = self.piece_min.as_nanos() as u64,
            "GC ran"
        );

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
            Some(DrainState::Drained(round)) => {
                self.end_round(round).await;
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
            self.end_round(draining.round()).await;
        }
    }

    async fn end_round(&mut self, round: u32) {
        self.highest_piece = 0;
        self.marked_to = 0;
        self.marked.clear();
        self.lost.clear();
        self.draining_round = None;

        trace!(round, "Ending round");

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
    /// A packet which will further transfer progress (like a piece).
    Data(IncomingData),
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
