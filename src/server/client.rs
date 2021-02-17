use crate::router;
use anyhow::Result;
use bytes::{Buf, BytesMut};
use libacp::proto;
use libacp::proto::packet::Data;
use libacp::proto::{Packet, Ping};
use prost::Message;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

pub struct Client {
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
    connection: Pin<Box<quiche::Connection>>,
    rx: UnboundedReceiver<BytesMut>,
    send_buf: BytesMut,
    stream_bufs: HashMap<u64, BytesMut>,
}

impl Client {
    pub fn new(
        socket: Arc<UdpSocket>,
        addr: SocketAddr,
        connection: Pin<Box<quiche::Connection>>,
        rx: UnboundedReceiver<BytesMut>,
    ) -> Self {
        let mut send_buf = BytesMut::with_capacity(router::DATAGRAM_SIZE);
        send_buf.resize(router::DATAGRAM_SIZE, 0);

        Client {
            socket,
            addr,
            connection,
            send_buf,
            rx,
            stream_bufs: HashMap::new(),
        }
    }

    pub async fn run(mut self) -> Result<()> {
        self.send().await?; // Acknowledge the established connection
        loop {
            self.recv().await?; // Receive new data into the connection
            self.send().await?; // Respond as necessary

            if self.connection.is_closed() {
                break;
            }

            if self.connection.is_established() && !self.connection.is_draining() {
                self.read_streams().await?;
            }
        }

        Ok(())
    }

    async fn read_streams(&mut self) -> Result<()> {
        const QUEUE_BUMP_SIZE: usize = 1024;

        println!("Reading streams");
        for stream_id in self.connection.readable() {
            println!("Receiving data for stream: {}", stream_id);
            let mut buf = self
                .stream_bufs
                .remove(&stream_id)
                .unwrap_or_else(|| BytesMut::new());
            let mut len = buf.len();
            buf.resize(len + QUEUE_BUMP_SIZE, 0);

            while let Ok((read, fin)) = self.connection.stream_recv(stream_id, &mut buf[len..]) {
                //TODO: Handle fin
                len += read;
                buf.resize(len + QUEUE_BUMP_SIZE, 0);
            }

            buf.resize(len, 0);

            while let Some(packet) = proto::frame(&mut buf).unwrap() {
                // TODO: Handle
                self.process(stream_id, packet).await?;
            }

            self.stream_bufs.insert(stream_id, buf);
        }

        Ok(())
    }

    async fn process(&mut self, stream_id: u64, packet: Packet) -> Result<()> {
        match packet.data.unwrap() {
            //TODO: Handle
            Data::Ping(ping) => {
                let response = Packet::new(Data::Ping(Ping { data: ping.data }));
                println!("Sending ping response");
                self.send_packet(stream_id, response).await
            }
        }
    }

    async fn close(&mut self, app: bool, err: u64, reason: &[u8]) -> Result<()> {
        self.connection.close(app, err, reason)?;
        self.send().await?;
        Ok(())
    }

    async fn send_packet(&mut self, stream_id: u64, packet: Packet) -> Result<()> {
        let mut buf = BytesMut::new();
        packet.encode_length_delimited(&mut buf)?;
        while buf.remaining() > 0 {
            buf.advance(self.connection.stream_send(stream_id, &buf[..], false)?);
        }

        self.send().await
    }

    async fn send(&mut self) -> Result<()> {
        loop {
            let len = match self.connection.send(&mut self.send_buf) {
                Ok(len) => len,
                Err(quiche::Error::Done) => {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };

            let mut sent = 0;
            while sent < len {
                let written = self
                    .socket
                    .send_to(&self.send_buf[sent..len], self.addr)
                    .await?;
                sent += written;
            }
        }
    }

    async fn recv(&mut self) -> std::result::Result<(), RecvError> {
        let socket_future = self.rx.recv();
        let bytes = match self.connection.timeout() {
            Some(duration) => match timeout(duration, socket_future).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    self.connection.on_timeout();
                    return Ok(());
                }
            },
            None => socket_future.await,
        };

        let mut bytes = match bytes {
            Some(bytes) => bytes,
            None => return Err(RecvError::DroppedChannel),
        };

        match self.connection.recv(&mut bytes) {
            Ok(len) => {
                println!("Client: QUIC read {} out of {}", len, bytes.len());
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Error, Debug)]
enum RecvError {
    #[error("client's receiver channel has been dropped")]
    DroppedChannel,
    #[error(transparent)]
    QuicError(#[from] quiche::Error),
}
