//! Types for handling outgoing file transfers

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ring::digest;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::proto::{
    datagram, packet, AckEndRound, BlockInfo, ControlUpdate, Datagram, EndRound, EndTransfer,
    OutgoingData, Packet, SendPiece,
};
use crate::Terminated;
use futures::FutureExt;

/// Type for managing information relating to an outgoing file transfer.
pub struct Outgoing {
    inner: Inner,
    rx: Receiver<IncomingPacket>,
    term_tx: Sender<Terminated>,
}

struct Inner {
    id: Vec<u8>,
    file: File,
    piece_size: u32,
    block_size: u64,
    tx: Sender<OutgoingData>,
    ctx: digest::Context,
    lost: BinaryHeap<Reverse<u64>>,
    round: Round,
    round_stall: bool,
    pieces_in_flight: u64,
    window_size: u64,
    target_window: u64,
}

impl Outgoing {
    /// Constructs a new outgoing file transfer.
    pub fn new(
        id: Vec<u8>,
        file: File,
        block_size: u32,
        piece_size: u32,
        tx: Sender<OutgoingData>,
        rx: Receiver<IncomingPacket>,
        term_tx: Sender<Terminated>,
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
                window_size: crate::INITIAL_WINDOW_SIZE,
                target_window: crate::INITIAL_WINDOW_SIZE,
            },
            rx,
            term_tx,
        }
    }

    /// Runs an outgoing file transfer.
    pub async fn run(mut self) {
        loop {
            if self.inner.round_stall || self.inner.pieces_in_flight >= self.inner.window_size {
                if self.await_update().await {
                    break;
                }
            } else {
                if let Some(true) = self.await_update().now_or_never() {
                    // Transfer finished
                    break;
                }

                self.inner.send_piece().await;
            }
        }

        self.term_tx
            .send(Terminated::Outgoing(self.inner.id))
            .await
            .unwrap();
    }

    async fn await_update(&mut self) -> bool {
        if let Some(update) = self.rx.recv().await {
            self.inner.apply_update(update)
        } else {
            false
        }
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
        self.tx.send(OutgoingData::Datagram(piece)).await.unwrap();
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
        self.tx.send(OutgoingData::Stream(info)).await.unwrap();
    }

    async fn end_round(&mut self) {
        println!("Ending round {}", self.round.num());
        self.tx
            .send(OutgoingData::Stream(Packet::new(packet::Data::EndRound(
                EndRound {
                    id: self.id.clone(),
                    round: self.round.num(),
                },
            ))))
            .await
            .unwrap();
        self.round_stall = true;
    }

    //TODO: Review return types
    fn apply_update(&mut self, update: IncomingPacket) -> bool {
        match update {
            IncomingPacket::ControlUpdate(update) => {
                // Decrement pieces in flight
                self.pieces_in_flight =
                    self.pieces_in_flight.saturating_sub(update.received as u64);
                self.pieces_in_flight = self
                    .pieces_in_flight
                    .saturating_sub(update.lost.len() as u64);

                self.target_window = update.window_size;
                self.window_size = std::cmp::min(
                    self.target_window,
                    self.window_size + update.received as u64,
                );

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

            IncomingPacket::EndTransfer(_) => {
                return true;
            }
        }

        false
    }
}

/// Network traffic routed into an outbound transfer.
#[derive(Debug)]
pub enum IncomingPacket {
    /// A flow control update for detecting lost packets and managing the sending window.
    ControlUpdate(ControlUpdate),
    /// The receiver is ready to start the next round.
    AckEndRound(AckEndRound),
    /// The receiver has terminated the transfer.
    EndTransfer(EndTransfer),
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
