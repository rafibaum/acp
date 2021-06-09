use crate::router;
use anyhow::Context;
use bytes::{Buf, BytesMut};
use libacp::mpsc as resizable;
use libacp::proto;
use libacp::proto::{
    datagram, packet, AcceptDownload, AcceptUpload, OutgoingData, SendPiece, StartBenchmark,
};
use libacp::proto::{Datagram, Packet, Ping};
use libacp::{incoming, outgoing};
use libacp::{Minmax, Terminated};
use prost::Message;
use quiche::{ConnectionId, RecvInfo};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, trace};

const RTT_WINDOW: Duration = Duration::from_secs(10);

pub struct Client {
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
    connection: Pin<Box<quiche::Connection>>,
    rx: Receiver<(BytesMut, RecvInfo)>,
    scid: ConnectionId<'static>,
    parent_term: Sender<ConnectionId<'static>>,
    send_buf: BytesMut,
    dgram_buf: BytesMut,
    stream_bufs: HashMap<u64, BytesMut>,
    state: ClientState,
    incoming: HashMap<Vec<u8>, IncomingHandle>,
    outgoing: HashMap<Vec<u8>, Sender<outgoing::IncomingPacket>>,
    out_tx: Sender<OutgoingData>,
    out_rx: Receiver<OutgoingData>,
    term_tx: Sender<Terminated>,
    term_rx: Receiver<Terminated>,
    rtt_filter: Minmax<Instant, u64>,
    rtt_min: Arc<AtomicU64>,
    dgram_slot: Option<BytesMut>,
    stream_slot: Option<(u64, BytesMut)>,
}

struct IncomingHandle {
    info_tx: Sender<incoming::IncomingPacket>,
    piece_tx: resizable::Sender<SendPiece>,
}

impl Client {
    pub fn new(
        socket: Arc<UdpSocket>,
        addr: SocketAddr,
        connection: Pin<Box<quiche::Connection>>,
        rx: Receiver<(BytesMut, RecvInfo)>,
        scid: ConnectionId<'static>,
        parent_term: Sender<ConnectionId<'static>>,
    ) -> Self {
        let mut send_buf = BytesMut::with_capacity(router::DATAGRAM_SIZE);
        send_buf.resize(router::DATAGRAM_SIZE, 0);

        let mut dgram_buf = BytesMut::with_capacity(router::DATAGRAM_SIZE);
        dgram_buf.resize(router::DATAGRAM_SIZE, 0);

        let (out_tx, out_rx) = mpsc::channel(64);

        let (term_tx, term_rx) = mpsc::channel(32);

        let rtt_min = connection.stats().rtt.as_nanos() as u64;

        Client {
            socket,
            addr,
            connection,
            send_buf,
            dgram_buf,
            rx,
            scid,
            parent_term,
            stream_bufs: HashMap::new(),
            state: ClientState::Connected,
            incoming: HashMap::new(),
            outgoing: HashMap::new(),
            out_tx,
            out_rx,
            rtt_min: Arc::new(AtomicU64::new(rtt_min)),
            rtt_filter: Minmax::new(Instant::now(), rtt_min),
            term_tx,
            term_rx,
            dgram_slot: None,
            stream_slot: None,
        }
    }

    pub async fn run(mut self) {
        let mut iter_arr = [0; 5];
        let mut iters = 0;

        self.send().await; // Acknowledge the established connection
        println!("Starting client");
        loop {
            let send_blocked = self.stream_slot.is_some() || self.dgram_slot.is_some();
            let rx = &mut self.rx;
            let timeout = self.connection.timeout();

            // Perform incoming actions
            tokio::select! {
                // Read incoming bytes
                bytes = rx.recv() => {
                    iter_arr[0] += 1;
                    self.recv(bytes);
                }

                // Wait for timeout
                true = async {
                    match timeout {
                        Some(timeout) => {
                            tokio::time::sleep(timeout).await;
                            true
                        }

                        None => false
                    }
                } => {
                    iter_arr[1] += 1;
                    self.connection.on_timeout();
                }

                // Send outgoing data
                Some(data) = self.out_rx.recv(), if !send_blocked => {
                    iter_arr[2] += 1;
                    match data {
                        OutgoingData::Stream(packet) => {
                            self.send_packet(0, packet).await;
                        }

                        OutgoingData::Datagram(datagram) => {
                            self.send_datagram(datagram).await;
                        }
                    }
                }

                // Handle cleanup
                Some(termination) = self.term_rx.recv() => {
                    iter_arr[3] += 1;
                    match termination {
                        Terminated::Outgoing(id) => {
                            self.outgoing.remove(&id);
                        }

                        Terminated::Incoming(id) => {
                            self.incoming.remove(&id);
                        }
                    }
                }

                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    iter_arr[4] += 1;
                    println!("Awaiting incoming");
                }
            }

            self.send().await; // Respond as necessary

            if self.connection.is_closed() {
                self.state = ClientState::Closed;
                debug!("client connection closed");
                break;
            }

            self.flush().await;

            if self.connection.is_established() && !self.connection.is_draining() {
                let new_min = self.rtt_filter.running_min(
                    RTT_WINDOW,
                    Instant::now(),
                    self.connection.stats().rtt.as_nanos() as u64,
                );
                if self.rtt_min.load(Ordering::Relaxed) != new_min {
                    self.rtt_min.store(new_min, Ordering::Relaxed);
                }

                self.read_streams().await;
                self.read_dgrams().await;
            }

            // if iters % 1000 == 0 {
            //     println!("Client Iters: {:?}", iter_arr);
            // }

            iters += 1;
        }

        println!("Ending client");

        if let Err(_) = self.parent_term.send(self.scid).await {
            error!("client termination channel dropped");
            panic!("client termination channel dropped");
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn read_streams(&mut self) {
        for stream_id in self.connection.readable() {
            let mut buf = self
                .stream_bufs
                .remove(&stream_id)
                .unwrap_or_else(BytesMut::new);
            self.read_stream(stream_id, &mut buf).await;
            self.stream_bufs.insert(stream_id, buf);
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn read_stream(&mut self, stream_id: u64, mut buf: &mut BytesMut) {
        const QUEUE_BUMP_SIZE: usize = 1024;

        let mut len = buf.len();
        buf.resize(len + QUEUE_BUMP_SIZE, 0);

        while let Ok((read, _fin)) = self.connection.stream_recv(stream_id, &mut buf[len..]) {
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
                    panic!("could not frame packet in stream")
                }
            };

            self.process(stream_id, packet).await;
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn read_dgrams(&mut self) {
        while let Ok(len) = self.connection.dgram_recv(&mut self.dgram_buf) {
            trace!(len, "received datagram");
            let datagram = Datagram::decode(&self.dgram_buf[..len])
                .context("unable to frame datagram")
                .unwrap();
            self.process_dgram(datagram).await;
        }
    }

    #[tracing::instrument(level = "trace", skip(self, stream_id, packet))]
    async fn process(&mut self, stream_id: u64, packet: Packet) {
        let data = match packet.data {
            Some(data) => data,
            None => {
                error!("packet sent without data field");
                panic!("missing data field");
            }
        };

        match data {
            packet::Data::Ping(ping) => {
                trace!(data = %&ping.data, "sending ping response");
                let response = Packet::new(packet::Data::Ping(Ping { data: ping.data }));
                self.send_packet(stream_id, response).await;
            }

            packet::Data::StartBenchmark(_) => match &self.state {
                ClientState::Connected => {
                    self.state = ClientState::Benchmarking(0);
                    trace!("starting benchmark");
                    let response = Packet::new(packet::Data::StartBenchmark(StartBenchmark {}));
                    self.send_packet(stream_id, response).await;
                }
                other => {
                    error!(state = ?other, "received a start benchmarking packet unexpectedly");
                    panic!("unexpected start benchmarking packet");
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
                    panic!("unexpected stop benchmarking packet");
                }
            },

            packet::Data::RequestUpload(info) => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(String::from_utf8(info.filename).unwrap())
                    .await
                    .unwrap();

                let (info_tx, info_rx) = mpsc::channel(64);
                let (piece_tx, piece_rx) = resizable::resizable(4);

                let incoming = incoming::Incoming::new(
                    info.id.clone(),
                    info_rx,
                    piece_rx,
                    self.out_tx.clone(),
                    file,
                    info.size,
                    info.block_size,
                    info.piece_size,
                    self.rtt_min.clone(),
                    self.term_tx.clone(),
                    info.options.unwrap().integrity,
                );

                self.incoming
                    .insert(info.id.clone(), IncomingHandle { info_tx, piece_tx });
                tokio::spawn(async move {
                    let start = Instant::now();
                    println!("Starting upload...");
                    incoming.run().await;
                    let dur = Instant::now().duration_since(start).as_millis();
                    println!("Took: {}ms", dur);
                });

                let accept = Packet::new(packet::Data::AcceptUpload(AcceptUpload { id: info.id }));
                self.send_packet(stream_id, accept).await;
            }

            packet::Data::BlockInfo(info) => match self.incoming.get(&info.id) {
                Some(incoming) => {
                    incoming
                        .info_tx
                        .send(incoming::IncomingPacket::BlockInfo(info))
                        .await
                        .unwrap();
                }

                None => {
                    panic!("Could not find transfer")
                }
            },

            packet::Data::EndRound(end) => match self.incoming.get(&end.id) {
                None => {
                    panic!("Could not find transfer")
                }

                Some(incoming) => {
                    incoming
                        .info_tx
                        .send(incoming::IncomingPacket::EndRound(end))
                        .await
                        .unwrap();
                }
            },

            packet::Data::RequestDownload(download) => {
                const BLOCK_SIZE: u32 = 2048;
                const PIECE_SIZE: u32 = 1024;

                let file = File::open(String::from_utf8(download.filename).unwrap())
                    .await
                    .unwrap();
                let filesize = file.metadata().await.unwrap().len();

                let (tx, rx) = mpsc::channel(64);
                let outgoing = outgoing::Outgoing::new(
                    download.id.clone(),
                    file.into_std().await,
                    BLOCK_SIZE,
                    PIECE_SIZE,
                    self.out_tx.clone(),
                    rx,
                    self.term_tx.clone(),
                    download.options.unwrap().integrity,
                );

                self.outgoing.insert(download.id.clone(), tx);
                tokio::spawn(async move {
                    let start = Instant::now();
                    println!("Starting download...");
                    outgoing.run().await;
                    let dur = Instant::now().duration_since(start).as_millis();
                    println!("Took: {}ms", dur);
                });

                let accept = Packet::new(packet::Data::AcceptDownload(AcceptDownload {
                    id: download.id,
                    size: filesize,
                    block_size: BLOCK_SIZE,
                    piece_size: PIECE_SIZE,
                }));
                self.send_packet(stream_id, accept).await;
            }

            packet::Data::ControlUpdate(update) => match self.outgoing.get(&update.id) {
                Some(outgoing) => {
                    outgoing
                        .send(outgoing::IncomingPacket::ControlUpdate(update))
                        .await
                        .unwrap();
                }

                None => panic!("Could not find transfer"),
            },

            packet::Data::AckEndRound(ack) => match self.outgoing.get(&ack.id) {
                Some(outgoing) => {
                    outgoing
                        .send(outgoing::IncomingPacket::AckEndRound(ack))
                        .await
                        .unwrap();
                }

                None => panic!("Could not find transfer"),
            },

            packet::Data::EndTransfer(end) => match self.outgoing.get(&end.id) {
                Some(outgoing) => {
                    outgoing
                        .send(outgoing::IncomingPacket::EndTransfer(end))
                        .await
                        .unwrap();
                }

                None => panic!("Could not find transfer"),
            },

            packet::Data::AcceptUpload(_) => unimplemented!(),
            packet::Data::AcceptDownload(_) => unimplemented!(),
        }
    }

    #[tracing::instrument(level = "trace", skip(self, datagram))]
    async fn process_dgram(&mut self, datagram: Datagram) {
        let data = match datagram.data {
            Some(data) => data,
            None => {
                error!("datagram sent without data field");
                panic!("datagram missing data field");
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
                        panic!("received benchmark payload in illegal state");
                    }
                }
            }

            datagram::Data::SendPiece(piece) => {
                if let Some(incoming) = self.incoming.get(&piece.id) {
                    // No need to do anything if this fails, just drop the piece silently
                    if let Err(e) = incoming.piece_tx.try_send(piece) {
                        match e {
                            TrySendError::Full(_) => println!("Piece dropped"),
                            TrySendError::Closed(_) => panic!("Piece channel closed"),
                        }
                    }
                }
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self, stream_id, packet))]
    async fn send_packet(&mut self, stream_id: u64, packet: Packet) {
        let mut buf = BytesMut::new();
        packet.encode_length_delimited(&mut buf).unwrap();
        self.send_packet_buf(stream_id, buf).await;
    }

    async fn send_packet_buf(&mut self, stream_id: u64, mut buf: BytesMut) {
        while buf.remaining() > 0 {
            let read = match self.connection.stream_send(stream_id, &buf[..], false) {
                // Same as done
                Ok(0) => {
                    if self.stream_slot.is_some() {
                        panic!("Tried sending packet while blocked");
                    } else {
                        self.stream_slot = Some((stream_id, buf));
                        return;
                    }
                }

                Ok(len) => len,

                Err(quiche::Error::Done) => {
                    if self.stream_slot.is_some() {
                        panic!("Tried sending packet while blocked");
                    } else {
                        self.stream_slot = Some((stream_id, buf));
                        return;
                    }
                }

                Err(e) => {
                    panic!("Error while sending packet: {}", e);
                }
            };

            buf.advance(read);
        }

        trace!("packet fully buffered in stream");
        self.send().await
    }

    async fn send_datagram(&mut self, datagram: Datagram) {
        let mut buf = BytesMut::new();
        datagram.encode(&mut buf).unwrap();
        self.send_datagram_buf(buf).await;
    }

    async fn send_datagram_buf(&mut self, buf: BytesMut) {
        match self.connection.dgram_send(&buf) {
            Ok(_) => {}
            Err(quiche::Error::Done) => {
                if self.dgram_slot.is_some() {
                    panic!("Tried sending datagram while blocked");
                } else {
                    self.dgram_slot = Some(buf);
                    return;
                }
            }
            Err(e) => {
                panic!("Error while sending datagram: {}", e);
            }
        }

        self.send().await;
    }

    async fn flush(&mut self) {
        if let Some(dgram) = self.dgram_slot.take() {
            self.send_datagram_buf(dgram).await;
        }

        if let Some((stream, packet)) = self.stream_slot.take() {
            self.send_packet_buf(stream, packet).await;
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn send(&mut self) {
        loop {
            let info = match self.connection.send(&mut self.send_buf) {
                Ok(info) => info,
                Err(quiche::Error::Done) => {
                    return;
                }
                Err(e) => {
                    error!(err = %e, "QUIC error while processing outgoing bytes");
                    panic!("QUIC error while processing outgoing bytes");
                }
            };

            let (len, info) = info;

            while info.at > Instant::now() {
                tokio::task::yield_now().await;
            }

            let mut sent = 0;
            while sent < len {
                let written = self
                    .socket
                    .send_to(&self.send_buf[sent..len], self.addr)
                    .await
                    .unwrap();
                trace!(written, length = len, "sent datagram to client");
                sent += written;
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self, bytes))]
    fn recv(&mut self, bytes: Option<(BytesMut, RecvInfo)>) {
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => {
                error!("client input channel dropped before client could safely terminate");
                panic!("client input channel dropped before client could safely terminate");
            }
        };
        let (bytes, info) = bytes;
        let mut bytes = bytes;

        trace!(len = bytes.len(), "client received bytes");

        match self.connection.recv(&mut bytes, info) {
            Ok(len) => {
                trace!(read = len, total = bytes.len(), "QUIC accepted input bytes");
            }
            Err(e) => {
                error!(err = %e, "QUIC error while processing incoming bytes");
                panic!("QUIC error while processing incoming bytes")
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
