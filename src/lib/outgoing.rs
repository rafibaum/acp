//! Types for handling outgoing file transfers

use crate::proto::{
    datagram, packet, AckEndRound, BlockInfo, ControlUpdate, Datagram, EndRound, Packet, SendPiece,
};
use futures::FutureExt;
use ring::digest;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc::{Sender, UnboundedReceiver};

/// Type for managing information relating to an outgoing file transfer.
pub struct Outgoing {
    id: Vec<u8>,
    file: File,
    piece_size: u32,
    block_size: u64,
    tx: Sender<OutgoingPacket>,
    ctx: digest::Context,
    window: u32,
    window_size: u32,
    max_window_size: u32,
    rx: UnboundedReceiver<IncomingPacket>,
    lost: BinaryHeap<Reverse<u64>>,
    round: Round,
    round_stall: bool,
}

impl Outgoing {
    /// Constructs a new outgoing file transfer.
    pub fn new(
        id: Vec<u8>,
        file: File,
        block_size: u32,
        piece_size: u32,
        tx: Sender<OutgoingPacket>,
        rx: UnboundedReceiver<IncomingPacket>,
    ) -> Self {
        Outgoing {
            id,
            file,
            piece_size,
            block_size: block_size as u64,
            tx,
            ctx: digest::Context::new(&digest::SHA256),
            window: 0,
            window_size: 1,
            max_window_size: 1,
            rx,
            lost: BinaryHeap::new(),
            round: Round::First { piece: 0 },
            round_stall: false,
        }
    }

    /// Runs an outgoing file transfer.
    pub async fn run(mut self) {
        'main: loop {
            'inner: loop {
                // Outer option indicates if there's a new flow control update to apply
                // Inner option is whether the receiving channel was closed (error condition)
                let update = if self.round_stall {
                    // Don't even try to send a piece, just wait for updates
                    Some(self.rx.recv().await)
                } else {
                    while self.window < self.window_size {
                        if self.send_piece().await {
                            // End of round, GOTO top again and wait for flow control updates
                            break 'inner;
                        }
                    }

                    match self.rx.recv().now_or_never() {
                        Some(update) => Some(update),
                        None => {
                            // No pending flow control updates, can we increase our window size?
                            if self.max_window_size > self.window_size {
                                // We can, let's send more stuff
                                self.window_size = self.max_window_size;
                                None
                            } else {
                                // We cannot, wait for a flow control update instead
                                Some(self.rx.recv().await)
                            }
                        }
                    }
                };

                if let Some(update) = update {
                    let update = update.expect("Incoming update channel unexpectedly dropped");
                    match update {
                        IncomingPacket::ControlUpdate(update) => {
                            self.apply_update(update);
                        }
                        IncomingPacket::AckEndRound(ack) => {
                            if ack.round != self.round.num() {
                                panic!("Received next round command for incorrect round");
                            }

                            if self.lost.is_empty() {
                                // All pieces received
                                break 'main;
                            }

                            let mut pieces = BinaryHeap::new();
                            std::mem::swap(&mut pieces, &mut self.lost);

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
                }
            }
        }

        println!("Lost: {:?}", self.lost);
    }

    fn apply_update(&mut self, update: ControlUpdate) {
        self.max_window_size = update.max_window_size;
        self.window -= update.received;
        self.window -= update.lost.len() as u32;
        self.lost.extend(update.lost.into_iter().map(Reverse));
    }

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

    async fn send_piece(&mut self) -> bool {
        let pair = match self.next_piece().await {
            Some(pair) => pair,
            None => {
                self.end_round().await;
                return true;
            }
        };

        let (piece_num, buf) = pair;

        let piece = Datagram::new(datagram::Data::SendPiece(SendPiece {
            id: self.id.clone(),
            piece: piece_num,
            window_size: self.window_size as u32,
            data: buf,
        }));
        self.tx.send(OutgoingPacket::Datagram(piece)).await.unwrap();
        self.window += 1;

        if let Round::First { .. } = self.round {
            if piece_num + 1 % self.block_size == 0 {
                let block = piece_num / self.piece_size as u64;
                println!("Done sending {}", block);
            }
        }

        false
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
}

/// A packet sent by an outgoing file transfer.
#[derive(Debug)]
pub enum OutgoingPacket {
    /// Outgoing stream packet.
    Stream(Packet),
    /// Outgoing datagram.
    Datagram(Datagram),
}

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
            Round::First { .. } => 1,
            Round::Retransmit { num, .. } => *num,
        }
    }
}
