//! Types for handling outgoing file transfers

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ring::digest;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::proto::{
    datagram, packet, AckEndRound, BlockInfo, ControlUpdate, Datagram, EndRound, Packet, SendPiece,
};

const INITIAL_WINDOW_SIZE: u64 = 4;

/// Type for managing information relating to an outgoing file transfer.
pub struct Outgoing {
    inner: Inner,
    rx: Receiver<IncomingPacket>,
}

struct Inner {
    id: Vec<u8>,
    file: File,
    piece_size: u32,
    block_size: u64,
    tx: Sender<OutgoingPacket>,
    ctx: digest::Context,
    lost: BinaryHeap<Reverse<u64>>,
    round: Round,
    round_stall: bool,
    pieces_in_flight: u64,
    window_size: u64,
}

impl Outgoing {
    /// Constructs a new outgoing file transfer.
    pub fn new(
        id: Vec<u8>,
        file: File,
        block_size: u32,
        piece_size: u32,
        tx: Sender<OutgoingPacket>,
        rx: Receiver<IncomingPacket>,
    ) -> Self {
        Outgoing {
            inner: Inner {
                id,
                file,
                piece_size,
                block_size: block_size as u64,
                tx,
                ctx: digest::Context::new(&digest::SHA256),
                lost: BinaryHeap::new(),
                round: Round::First { piece: 0 },
                round_stall: false,
                pieces_in_flight: 0,
                window_size: INITIAL_WINDOW_SIZE,
            },
            rx,
        }
    }

    /// Runs an outgoing file transfer.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                //TODO: Reevaluate use of select (futures can be cancelled)
                _ = self.inner.send_piece(), if !self.inner.round_stall && self.inner.pieces_in_flight < self.inner.window_size => {}

                Some(update) = self.rx.recv() => {
                    if self.inner.apply_update(update) {
                        break;
                    }
                }
            }
        }

        println!("Lost: {:?}", self.inner.lost);
    }
}

impl Inner {
    async fn next_piece(&mut self) -> Option<(u64, Vec<u8>)> {
        let mut buf = vec![0; self.piece_size as usize];

        // Identify next piece to read and seek if necessary
        let piece = match &mut self.round {
            Round::First { piece } => {
                // First round pieces are read sequentially, no seeking needed
                *piece
            }

            Round::Retransmit {
                num: _,
                ref mut pieces,
            } => match pieces.pop() {
                None => return None,
                Some(Reverse(piece)) => {
                    self.file
                        .seek(SeekFrom::Start(piece * self.piece_size as u64))
                        .await
                        .unwrap();
                    piece
                }
            },
        };

        // Read piece into buffer
        let mut read = 0;
        loop {
            let len = self.file.read(&mut buf[read..]).await.unwrap();
            read += len;

            if read >= buf.len() || len == 0 {
                // Buffer is full or we've reached EOF
                buf.truncate(read);
                break;
            }
        }

        // First round transmission calculations
        if let Round::First { piece } = &mut self.round {
            // Reached EOF
            if read == 0 {
                if *piece % self.block_size != 0 {
                    let block = (*piece / self.block_size) as u32;
                    self.finish_block(block).await;
                }

                return None;
            }

            // Update digest
            self.ctx.update(&buf);

            // Bump piece number
            *piece += 1;
            if *piece % self.block_size == 0 {
                let block = (*piece / self.block_size - 1) as u32;
                self.finish_block(block).await;
            }
        }

        Some((piece, buf))
    }

    async fn send_piece(&mut self) {
        let pair = match self.next_piece().await {
            Some(pair) => pair,
            None => {
                self.end_round().await;
                return;
            }
        };

        let (piece_num, buf) = pair;

        let piece = Datagram::new(datagram::Data::SendPiece(SendPiece {
            id: self.id.clone(),
            piece: piece_num,
            data: buf,
        }));
        self.tx.send(OutgoingPacket::Datagram(piece)).await.unwrap();
        self.pieces_in_flight += 1;

        if let Round::First { .. } = self.round {
            if piece_num + 1 % self.block_size == 0 {
                let block = piece_num / self.piece_size as u64;
                println!("Done sending {}", block);
            }
        }
    }

    async fn finish_block(&mut self, block: u32) {
        let mut ctx = digest::Context::new(&digest::SHA256);
        std::mem::swap(&mut ctx, &mut self.ctx);
        let checksum = ctx.finish();

        let info = Packet::new(packet::Data::BlockInfo(BlockInfo {
            id: self.id.clone(),
            block,
            checksum: Vec::from(checksum.as_ref()),
        }));
        self.tx.send(OutgoingPacket::Stream(info)).await.unwrap();
    }

    async fn end_round(&mut self) {
        println!("Ending round {}", self.round.num());
        self.tx
            .send(OutgoingPacket::Stream(Packet::new(packet::Data::EndRound(
                EndRound {
                    id: self.id.clone(),
                    round: self.round.num(),
                },
            ))))
            .await
            .unwrap();
        self.round_stall = true;
    }

    fn apply_update(&mut self, update: IncomingPacket) -> bool {
        match update {
            IncomingPacket::ControlUpdate(update) => {
                // Decrement pieces in flight
                self.pieces_in_flight =
                    self.pieces_in_flight.saturating_sub(update.received as u64);
                self.pieces_in_flight = self
                    .pieces_in_flight
                    .saturating_sub(update.lost.len() as u64);

                self.window_size = update.window_size;

                self.lost.extend(update.lost.into_iter().map(Reverse));
            }
            IncomingPacket::AckEndRound(ack) => {
                if ack.round != self.round.num() {
                    panic!("Received next round command for incorrect round");
                }

                if self.lost.is_empty() {
                    // All pieces received
                    return true;
                }

                let mut pieces = BinaryHeap::new();
                std::mem::swap(&mut pieces, &mut self.lost);

                println!("Starting next round");

                self.round = match self.round {
                    Round::First { .. } => Round::Retransmit { num: 1, pieces },
                    Round::Retransmit { num, .. } => Round::Retransmit {
                        num: num + 1,
                        pieces,
                    },
                };

                self.round_stall = false;
            }
        }

        false
    }
}

/// A packet sent by an outgoing file transfer.
#[derive(Debug)]
pub enum OutgoingPacket {
    /// Outgoing stream packet.
    Stream(Packet),
    /// Outgoing datagram.
    Datagram(Datagram),
}

#[derive(Debug)]
pub enum IncomingPacket {
    ControlUpdate(ControlUpdate),
    AckEndRound(AckEndRound),
}

enum Round {
    First {
        piece: u64,
    },
    Retransmit {
        num: u32,
        pieces: BinaryHeap<Reverse<u64>>,
    },
}

impl Round {
    fn num(&self) -> u32 {
        match self {
            Round::First { .. } => 0,
            Round::Retransmit { num, .. } => *num,
        }
    }
}
