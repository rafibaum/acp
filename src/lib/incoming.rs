//! Types for handling incoming file transfers.

use crate::proto;
use bitvec::vec::BitVec;
use ring::digest;
use std::collections::HashMap;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Type for managing information relating to an incoming file transfer.
pub struct Incoming {
    file: File,
    blocks: HashMap<usize, BlockInfo>,
    /// Number of blocks in this file
    num_blocks: usize,
    /// Size of each block in bytes
    block_size: usize,
    /// Size of each piece in bytes
    piece_size: usize,
    //TODO: Add reasonable buffer sizes
    rx: UnboundedReceiver<IncomingPacket>,
    tx: UnboundedSender<proto::AckPiece>,
    received: usize,
}

struct BlockInfo {
    num: usize,
    /// Block offset from the start of the file in bytes
    offset: usize,
    /// Size of each piece in the block
    piece_size: usize,
    progress: BitVec,
    /// Number of pieces received
    received: usize,
    //TODO: Replace below with struct that captures both aspects
    checksum: Option<Vec<u8>>,
    /// Total number of pieces
    total: Option<usize>,
}

impl Incoming {
    /// Constructs a new incoming file transfer.
    pub fn new(
        rx: UnboundedReceiver<IncomingPacket>,
        tx: UnboundedSender<proto::AckPiece>,
        file: File,
        blocks_cap: usize,
        block_size: usize,
        piece_size: usize,
    ) -> Self {
        Incoming {
            file,
            blocks: HashMap::with_capacity(blocks_cap),
            num_blocks: blocks_cap,
            block_size,
            piece_size,
            rx,
            tx,
            received: 0,
        }
    }

    /// Starts an incoming file transfer.
    pub async fn run(mut self) {
        loop {
            let packet = match self.rx.recv().await {
                Some(packet) => packet,
                None => break,
            };

            let block_num = match &packet {
                IncomingPacket::BlockInfo(info) => info.block,
                IncomingPacket::Piece(piece) => piece.block,
            } as usize;

            // Get block or insert empty block information
            //TODO: Replace with better solution, borrow checker annoying here
            let block_size = self.block_size;
            let piece_size = self.piece_size;
            let block = self.blocks.entry(block_num).or_insert_with(|| {
                let offset = block_num * block_size;
                BlockInfo::new(block_num, offset, piece_size)
            });

            let result = match packet {
                // Attach block information as it's received
                IncomingPacket::BlockInfo(info) => {
                    block.attach_info(info.pieces as usize, info.checksum)
                }

                // Process incoming piece
                IncomingPacket::Piece(piece) => {
                    let result = block
                        .write_piece(piece.piece as usize, &mut self.file, &piece.data)
                        .await;

                    //TODO: Replace with real flow control
                    self.tx
                        .send(proto::AckPiece {
                            id: piece.id,
                            block: piece.block,
                            piece: piece.piece,
                        })
                        .unwrap();

                    result
                }
            };

            // Check if we now have a complete block
            if let BlockStatus::Done = result {
                if block.verify(&mut self.file).await {
                    self.received += 1;
                    println!(
                        "Verified block {}, done {} of {}",
                        block.num, self.received, self.num_blocks
                    );

                    if self.received >= self.num_blocks {
                        break;
                    }
                } else {
                    //TODO: Proper checksum mismatch handling
                    panic!("Does not match");
                }
            }
        }
    }
}

impl BlockInfo {
    fn new(num: usize, offset: usize, piece_size: usize) -> Self {
        BlockInfo {
            num,
            offset,
            piece_size,
            progress: BitVec::new(),
            received: 0,
            checksum: None,
            total: None,
        }
    }

    async fn write_piece(&mut self, piece: usize, file: &mut File, data: &[u8]) -> BlockStatus {
        match self.total {
            Some(total) => {
                if piece >= total {
                    //TODO: Handle OOB pieces
                    panic!("Piece OOB")
                }
            }
            None => {
                if piece >= self.progress.len() {
                    self.progress.resize(piece + 1, false);
                }
            }
        }

        if self.progress[piece] {
            //TODO: Handle rereceived pieces
            panic!("Piece already received");
        }

        self.progress.set(piece, true);
        self.received += 1;

        let offset = (self.offset + piece * self.piece_size) as u64;
        file.seek(SeekFrom::Start(offset)).await.unwrap();
        file.write_all(&data).await.unwrap();

        self.check_done()
    }

    fn attach_info(&mut self, total: usize, digest: Vec<u8>) -> BlockStatus {
        self.total = Some(total);
        self.checksum = Some(digest);
        //TODO: Check if longer
        self.progress.resize(total, false);

        self.check_done()
    }

    fn check_done(&self) -> BlockStatus {
        match self.total {
            None => BlockStatus::Pending,
            Some(total) => {
                if self.received >= total {
                    BlockStatus::Done
                } else {
                    BlockStatus::Pending
                }
            }
        }
    }

    async fn verify(&mut self, file: &mut File) -> bool {
        let checksum = match &self.checksum {
            Some(checksum) => checksum,
            None => return false,
        };
        let total = self.total.unwrap() * self.piece_size; // Above only exists if this does too

        let mut ctx = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0 as u8; 65535];
        file.seek(SeekFrom::Start(self.offset as u64))
            .await
            .unwrap();

        let mut read = 0;
        loop {
            let size = std::cmp::min(buf.len(), total - read);
            let len = file.read(&mut buf[..size]).await.unwrap();

            //TODO: Validate piece size
            if len == 0 {
                break;
            }

            read += len;
            ctx.update(&mut buf[..len]);

            if read >= total {
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
    /// An incoming piece.
    Piece(proto::SendPiece),
    /// Incoming information for a block.
    BlockInfo(proto::BlockInfo),
}
