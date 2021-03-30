//! Types for handling outgoing file transfers

use crate::proto::{datagram, packet, BlockInfo, Datagram, Packet, SendPiece};
use ring::digest;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::Sender;

/// Type for managing information relating to an outgoing file transfer.
pub struct Outgoing {
    id: Vec<u8>,
    file: File,
    pieces_per_block: usize,
    piece_size: usize,
    block: usize,
    piece: usize,
    tx: Sender<OutgoingPacket>,
    ctx: digest::Context,
}

impl Outgoing {
    /// Constructs a new outgoing file transfer.
    pub fn new(
        id: Vec<u8>,
        file: File,
        block_size: usize,
        piece_size: usize,
        tx: Sender<OutgoingPacket>,
    ) -> Self {
        Outgoing {
            id,
            file,
            pieces_per_block: block_size / piece_size,
            piece_size,
            block: 0,
            piece: 0,
            tx,
            ctx: digest::Context::new(&digest::SHA256),
        }
    }

    /// Runs an outgoing file transfer.
    pub async fn run(mut self) {
        loop {
            let mut buf = vec![0 as u8; self.piece_size];

            let mut read = 0;
            loop {
                let len = self.file.read(&mut buf[read..]).await.unwrap();
                read += len;

                if read >= buf.len() || len == 0 {
                    // If buffer is full or we've reached EOF
                    buf.resize(read, 0);
                    break;
                }
            }

            if read == 0 {
                if self.piece != 0 {
                    println!("Done and EOF sending {}", self.block);

                    self.finish_block().await;
                }
                break;
            }

            self.ctx.update(&buf);

            let piece = Datagram::new(datagram::Data::SendPiece(SendPiece {
                id: self.id.clone(),
                block: self.block as u32,
                piece: self.piece as u32,
                data: buf,
            }));
            self.tx.send(OutgoingPacket::Datagram(piece)).await.unwrap();

            self.piece += 1;
            if self.piece == self.pieces_per_block {
                self.finish_block().await;
                println!("Done sending {}", self.block);

                self.block += 1;
                self.piece = 0;
            }
        }
    }

    async fn finish_block(&mut self) {
        let mut ctx = digest::Context::new(&digest::SHA256);
        std::mem::swap(&mut ctx, &mut self.ctx);
        let checksum = ctx.finish();

        let info = Packet::new(packet::Data::BlockInfo(BlockInfo {
            id: self.id.clone(),
            block: self.block as u32,
            pieces: self.piece as u32,
            checksum: Vec::from(checksum.as_ref()),
        }));
        self.tx.send(OutgoingPacket::Stream(info)).await.unwrap();
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
