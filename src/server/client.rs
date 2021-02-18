use crate::{router, AcpServerError};
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
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;
use tracing::{debug, error, trace};

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
                debug!("client connection closed");
                break;
            }

            if self.connection.is_established() && !self.connection.is_draining() {
                self.read_streams().await?;
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn read_streams(&mut self) -> Result<()> {
        for stream_id in self.connection.readable() {
            self.read_stream(stream_id).await?;
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn read_stream(&mut self, stream_id: u64) -> Result<()> {
        const QUEUE_BUMP_SIZE: usize = 1024;

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
        trace!(len, "read new data from QUIC stream");

        loop {
            let packet = match proto::frame(&mut buf) {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(e) => {
                    error!(err = %e, "could not frame packet in stream");
                    return Err(e.into());
                }
            };

            self.process(stream_id, packet).await?;
        }

        self.stream_bufs.insert(stream_id, buf);

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, stream_id, packet))]
    async fn process(&mut self, stream_id: u64, packet: Packet) -> Result<()> {
        let data = match packet.data {
            Some(data) => data,
            None => {
                error!("packet sent without data field");
                return Err(AcpServerError::ChannelDropped.into());
            }
        };

        match data {
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

    #[tracing::instrument(level = "trace", skip(self, stream_id, packet))]
    async fn send_packet(&mut self, stream_id: u64, packet: Packet) -> Result<()> {
        let mut buf = BytesMut::new();
        packet.encode_length_delimited(&mut buf)?;
        while buf.remaining() > 0 {
            buf.advance(self.connection.stream_send(stream_id, &buf[..], false)?);
        }

        trace!("packet fully buffered in stream");
        self.send().await
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn send(&mut self) -> Result<()> {
        loop {
            let len = match self.connection.send(&mut self.send_buf) {
                Ok(len) => len,
                Err(quiche::Error::Done) => {
                    return Ok(());
                }
                Err(e) => {
                    error!(err = %e, "QUIC error while processing outgoing bytes");
                    return Err(e.into());
                }
            };

            let mut sent = 0;
            while sent < len {
                let written = self
                    .socket
                    .send_to(&self.send_buf[sent..len], self.addr)
                    .await?;
                trace!(written, "sent datagram to client");
                sent += written;
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn recv(&mut self) -> Result<()> {
        let socket_future = self.rx.recv();
        let bytes = match self.connection.timeout() {
            Some(duration) => match timeout(duration, socket_future).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    trace!("timeout lapsed");
                    self.connection.on_timeout();
                    return Ok(());
                }
            },
            None => socket_future.await,
        };

        let mut bytes = match bytes {
            Some(bytes) => bytes,
            None => {
                error!("client input channel dropped before client could safely terminate");
                return Err(AcpServerError::ChannelDropped.into());
            }
        };
        trace!(len = bytes.len(), "client received bytes");

        match self.connection.recv(&mut bytes) {
            Ok(len) => {
                trace!(read = len, total = bytes.len(), "QUIC accepted input bytes");
                Ok(())
            }
            Err(e) => {
                error!(err = %e, "QUIC error while processing incoming bytes");
                Err(e.into())
            }
        }
    }
}
