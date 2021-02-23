use crate::{router, AcpServerError};
use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use libacp::proto::{datagram, packet, StartBenchmark};
use libacp::proto::{Datagram, Packet, Ping};
use libacp::{proto, AcpError};
use prost::Message;
use quiche::ConnectionId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use tracing::{debug, error, trace};

pub struct Client {
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
    connection: Pin<Box<quiche::Connection>>,
    rx: UnboundedReceiver<BytesMut>,
    scid: ConnectionId<'static>,
    term_tx: UnboundedSender<ConnectionId<'static>>,
    send_buf: BytesMut,
    dgram_buf: BytesMut,
    stream_bufs: HashMap<u64, BytesMut>,
    state: ClientState,
}

impl Client {
    pub fn new(
        socket: Arc<UdpSocket>,
        addr: SocketAddr,
        connection: Pin<Box<quiche::Connection>>,
        rx: UnboundedReceiver<BytesMut>,
        scid: ConnectionId<'static>,
        term_tx: UnboundedSender<ConnectionId<'static>>,
    ) -> Self {
        let mut send_buf = BytesMut::with_capacity(router::DATAGRAM_SIZE);
        send_buf.resize(router::DATAGRAM_SIZE, 0);

        let mut dgram_buf = BytesMut::with_capacity(router::DATAGRAM_SIZE);
        dgram_buf.resize(router::DATAGRAM_SIZE, 0);

        Client {
            socket,
            addr,
            connection,
            send_buf,
            dgram_buf,
            rx,
            scid,
            term_tx,
            stream_bufs: HashMap::new(),
            state: ClientState::Connected,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        self.send().await?; // Acknowledge the established connection
        loop {
            self.recv().await?; // Receive new data into the connection
            self.send().await?; // Respond as necessary

            if self.connection.is_closed() {
                self.state = ClientState::Closed;
                debug!("client connection closed");
                break;
            }

            if self.connection.is_established() && !self.connection.is_draining() {
                self.read_streams().await?;
                self.read_dgrams().await?;
            }
        }

        if let Err(e) = self.term_tx.send(self.scid) {
            error!("client termination channel dropped");
            return Err(e).context("client termination channel dropped");
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

        while let Ok((read, _fin)) = self.connection.stream_recv(stream_id, &mut buf[len..]) {
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

    #[tracing::instrument(level = "trace", skip(self))]
    async fn read_dgrams(&mut self) -> Result<()> {
        while let Ok(len) = self.connection.dgram_recv(&mut self.dgram_buf) {
            trace!(len, "received datagram");
            let datagram =
                Datagram::decode(&self.dgram_buf[..len]).context("unable to frame datagram")?;
            self.process_dgram(datagram).await?;
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, stream_id, packet))]
    async fn process(&mut self, stream_id: u64, packet: Packet) -> Result<()> {
        let data = match packet.data {
            Some(data) => data,
            None => {
                error!("packet sent without data field");
                return Err(AcpError::InvalidPacket).context("missing data field");
            }
        };

        match data {
            packet::Data::Ping(ping) => {
                trace!(data = %&ping.data, "sending ping response");
                let response = Packet::new(packet::Data::Ping(Ping { data: ping.data }));
                self.send_packet(stream_id, response).await?;
            }

            packet::Data::StartBenchmark(_) => match &self.state {
                ClientState::Connected => {
                    self.state = ClientState::Benchmarking(0);
                    trace!("starting benchmark");
                    let response = Packet::new(packet::Data::StartBenchmark(StartBenchmark {}));
                    self.send_packet(stream_id, response).await?;
                }
                other => {
                    error!(state = ?other, "received a start benchmarking packet unexpectedly");
                    return Err(AcpError::IllegalPacket)
                        .context("unexpected start benchmarking packet");
                }
            },

            packet::Data::StopBenchmark(_) => match &self.state {
                ClientState::Benchmarking(data) => {
                    trace!(data, "stopping benchmark");
                    println!("Benchmark stopped after: {}", data);
                    self.state = ClientState::Connected;
                }
                other => {
                    error!(state = ?other, "received a stop benchmarking packet unexpectedly");
                    return Err(AcpError::IllegalPacket)
                        .context("unexpected stop benchmarking packet");
                }
            },
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, datagram))]
    async fn process_dgram(&mut self, datagram: Datagram) -> Result<()> {
        let data = match datagram.data {
            Some(data) => data,
            None => {
                error!("datagram sent without data field");
                return Err(AcpError::InvalidPacket).context("datagram missing data field");
            }
        };

        match data {
            datagram::Data::BenchmarkPayload(payload) => {
                let payload = payload.payload;
                match &mut self.state {
                    ClientState::Benchmarking(data) => {
                        *data += payload.len() as u64;
                        trace!(
                            cumulative = *data,
                            payload = payload.len(),
                            "received benchmark payload"
                        );
                    }
                    _ => {
                        error!("received benchmark payload in illegal state");
                        return Err(AcpError::IllegalPacket)
                            .context("received benchmark payload in illegal state");
                    }
                }
            }
        }

        Ok(())
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
                    return Err(e).context("QUIC error while processing outgoing bytes");
                }
            };

            let mut sent = 0;
            while sent < len {
                let written = self
                    .socket
                    .send_to(&self.send_buf[sent..len], self.addr)
                    .await?;
                trace!(written, length = len, "sent datagram to client");
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
                return Err(AcpServerError::ChannelDropped)
                    .context("client input channel dropped before client could safely terminate");
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
                Err(e).context("QUIC error while processing incoming bytes")
            }
        }
    }
}

#[derive(Debug)]
enum ClientState {
    Connected,
    Closed,
    Benchmarking(u64),
}
