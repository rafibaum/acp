//! ACP client.
//!
//! A client for `acp` servers.

#![warn(missing_docs)]

use anyhow::Result;
use bytes::{Buf, BytesMut};
use clap::ArgMatches;
use libacp::incoming::Incoming;
use libacp::mpsc as resizable;
use libacp::outgoing::Outgoing;
use libacp::proto::packet::Data;
use libacp::proto::packets;
use libacp::proto::{datagram, AcceptDownload, OutgoingData, RequestDownload, SendPiece};
use libacp::proto::{Datagram, Packet, RequestUpload};
use libacp::{incoming, outgoing, proto};
use libacp::{Minmax, Terminated};
use prost::Message;
use quiche::{Connection, ConnectionId, RecvInfo};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;

mod cli;

const RTT_WINDOW: Duration = Duration::from_secs(10);

/// Client's main function.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::build_cli();

    let matches = cli.get_matches();

    let server = matches.value_of("SERVER").unwrap_or("127.0.0.1:55280");

    let command = if let Some(matches) = matches.subcommand_matches("get") {
        let options = TransferOptions::from_matches(matches);

        Command::GetFile {
            local: matches.value_of("DESTINATION").unwrap().to_owned(),
            remote: matches.value_of("SOURCE").unwrap().to_owned(),
            options,
        }
    } else if let Some(matches) = matches.subcommand_matches("put") {
        let options = TransferOptions::from_matches(matches);

        Command::PutFile {
            local: matches.value_of("SOURCE").unwrap().to_owned(),
            remote: matches.value_of("DESTINATION").unwrap().to_owned(),
            options,
        }
    } else {
        panic!("No subcommand found!");
    };

    let sock = UdpSocket::bind("0.0.0.0:55281").await.unwrap();
    let server = server.to_socket_addrs().unwrap().next().unwrap();
    sock.connect(server).await.unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_application_protos(b"\x07acp/0.1").unwrap();
    config.verify_peer(false);
    config.set_max_recv_udp_payload_size(1200);
    config.set_max_send_udp_payload_size(1200);
    config.set_initial_max_data(1000000);
    config.set_initial_max_streams_bidi(1);
    config.set_initial_max_streams_uni(1);
    config.set_initial_max_stream_data_bidi_local(1000000);
    config.set_initial_max_stream_data_bidi_remote(1000000);
    config.set_initial_max_stream_data_uni(1000000);
    config.set_max_idle_timeout(30 * 1000);
    config.enable_dgram(true, 512, 512);
    config
        .set_cc_algorithm_name(matches.value_of("congestion control").unwrap())
        .unwrap();

    let rng = ring::rand::SystemRandom::new();
    let mut scid = vec![0; quiche::MAX_CONN_ID_LEN];
    rng.fill(&mut scid).unwrap();
    let scid = ConnectionId::from_vec(scid);

    let conn = quiche::connect(None, &scid, server, &mut config).unwrap();

    //TODO: Adjust buffer sizes
    let (tx, rx) = mpsc::channel(32);
    let client = Client::new(conn, sock, rx, rng, server);

    tx.send(command).await.unwrap();
    std::mem::drop(tx);

    client.run().await;

    Ok(())
}

struct Client {
    inner: Inner,
    out_rx: Receiver<OutgoingData>,
    stream_bufs: HashMap<u64, BytesMut>,
    term_rx: Receiver<Terminated>,
}

struct Inner {
    connection: Pin<Box<Connection>>,
    socket: UdpSocket,
    send_buf: BytesMut,
    recv_buf: BytesMut,
    commands: Receiver<Command>,
    rng: SystemRandom,
    outgoing: OutgoingTransfers,
    incoming: IncomingTransfers,
    out_tx: Sender<OutgoingData>,
    dgram_buf: BytesMut,
    rtt_min: Arc<AtomicU64>,
    rtt_filter: Minmax<Instant, u64>,
    term_tx: Sender<Terminated>,
    dgram_slot: Option<BytesMut>,
    stream_slot: Option<(u64, BytesMut)>,
    recv_info: RecvInfo,
}

struct OutgoingTransfers {
    transfers: HashMap<Vec<u8>, Sender<outgoing::IncomingPacket>>,
    pending: HashMap<Vec<u8>, oneshot::Sender<Receiver<outgoing::IncomingPacket>>>,
}

struct IncomingTransfers {
    transfers: HashMap<Vec<u8>, IncomingHandle>,
    pending: HashMap<Vec<u8>, oneshot::Sender<DownloadStartHandle>>,
}

#[derive(Debug)]
struct DownloadStartHandle {
    info_rx: Receiver<incoming::IncomingPacket>,
    piece_rx: resizable::Receiver<SendPiece>,
    info: AcceptDownload,
}

struct IncomingHandle {
    info_tx: Sender<incoming::IncomingPacket>,
    piece_tx: resizable::Sender<SendPiece>,
}

impl Client {
    pub fn new(
        connection: Pin<Box<Connection>>,
        socket: UdpSocket,
        commands: Receiver<Command>,
        rng: SystemRandom,
        bound_addr: SocketAddr,
    ) -> Self {
        let mut send_buf = BytesMut::with_capacity(65535);
        send_buf.resize(65535, 0);

        let mut recv_buf = BytesMut::with_capacity(65535);
        recv_buf.resize(65535, 0);

        let mut dgram_buf = BytesMut::with_capacity(65535);
        dgram_buf.resize(65535, 0);

        let (out_tx, out_rx) = mpsc::channel(32);

        let (term_tx, term_rx) = mpsc::channel(32);

        let rtt = connection.stats().rtt.as_nanos() as u64;

        Client {
            inner: Inner {
                connection,
                socket,
                send_buf,
                recv_buf,
                commands,
                rng,
                outgoing: OutgoingTransfers {
                    transfers: HashMap::new(),
                    pending: HashMap::new(),
                },
                incoming: IncomingTransfers {
                    transfers: HashMap::new(),
                    pending: HashMap::new(),
                },
                out_tx,
                dgram_buf,
                rtt_min: Arc::new(AtomicU64::new(rtt)),
                rtt_filter: Minmax::new(Instant::now(), rtt),
                term_tx,
                dgram_slot: None,
                stream_slot: None,
                recv_info: RecvInfo { from: bound_addr },
            },
            stream_bufs: HashMap::new(),
            out_rx,
            term_rx,
        }
    }

    pub async fn run(mut self) {
        // Initialise connection
        self.inner.send().await;
        loop {
            // TODO: Timeout
            self.inner.recv().await;
            self.inner.send().await;

            if self.inner.connection.is_closed() {
                println!("Could not establish connection");
            }

            if self.inner.connection.is_established() && !self.inner.connection.is_draining() {
                break;
            }
        }

        // Initialised, start processing commands
        while let Some(command) = self.inner.commands.recv().await {
            let handle = match command {
                Command::PutFile {
                    local,
                    remote,
                    options,
                } => {
                    let file = File::open(local).await.expect("File not found");

                    let mut id = vec![0; 16];
                    self.inner.rng.fill(&mut id).unwrap();

                    let (start_tx, start_rx) = oneshot::channel();
                    self.inner.outgoing.pending.insert(id.clone(), start_tx);

                    let packet = Packet::new(Data::RequestUpload(RequestUpload {
                        id: id.clone(),
                        filename: remote.into_bytes(),
                        size: file.metadata().await.unwrap().len(),
                        block_size: 2048,
                        piece_size: 1024,
                        options: Some(packets::TransferOptions {
                            integrity: options.integrity,
                        }),
                    }));
                    self.inner.send_packet(0, packet).await;

                    let out_tx = self.inner.out_tx.clone();
                    let term_tx = self.inner.term_tx.clone();

                    tokio::spawn(async move {
                        let in_rx = start_rx.await.unwrap();
                        let outgoing = Outgoing::new(
                            id,
                            file.into_std().await,
                            2048,
                            1024,
                            out_tx,
                            in_rx,
                            term_tx,
                            options.integrity,
                        );
                        outgoing.run().await
                    })
                }

                Command::GetFile {
                    local,
                    remote,
                    options,
                } => {
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .open(local)
                        .await
                        .unwrap();

                    let mut id = vec![0; 16];
                    self.inner.rng.fill(&mut id).unwrap();

                    let (start_tx, start_rx) = oneshot::channel();
                    self.inner.incoming.pending.insert(id.clone(), start_tx);

                    let packet = Packet::new(Data::RequestDownload(RequestDownload {
                        id: id.clone(),
                        filename: remote.into_bytes(),
                        options: Some(packets::TransferOptions {
                            integrity: options.integrity,
                        }),
                    }));
                    self.inner.send_packet(0, packet).await;

                    let in_tx = self.inner.out_tx.clone();
                    let rtt_min = self.inner.rtt_min.clone();
                    let term_tx = self.inner.term_tx.clone();

                    tokio::spawn(async move {
                        let start = start_rx.await.unwrap();
                        let incoming = Incoming::new(
                            id,
                            start.info_rx,
                            start.piece_rx,
                            in_tx,
                            file,
                            start.info.size,
                            start.info.block_size,
                            start.info.piece_size,
                            rtt_min,
                            term_tx,
                            options.integrity,
                        );
                        incoming.run().await
                    })
                }
            };

            tokio::pin!(handle);

            // Process network traffic until command finished
            loop {
                let should_send_out =
                    self.inner.dgram_slot.is_none() && self.inner.stream_slot.is_none();
                let timeout = self.inner.connection.timeout();

                tokio::select! {
                    // Command finished
                    _ = &mut handle => break,

                    // Incoming traffic
                    _ = self.inner.recv() => {
                        self.inner.send().await;

                        if self.inner.connection.is_closed() || self.inner.connection.is_draining() {
                            // Connection ending
                            break;
                        }

                        self.inner.flush().await;

                        let new_min = self.inner.rtt_filter.running_min(RTT_WINDOW, Instant::now(), self.inner.connection.stats().rtt.as_nanos() as u64);
                        if new_min != self.inner.rtt_min.load(Ordering::Relaxed) {
                            self.inner.rtt_min.store(new_min, Ordering::Relaxed);
                        }

                        self.read_streams().await;
                        self.inner.read_dgrams().await;
                    }

                    // Timeout
                    true = async {
                        match timeout {
                            Some(timeout) => {
                                tokio::time::sleep(timeout).await;
                                true
                            }

                            None => false
                        }
                    } => {
                        self.inner.connection.on_timeout();
                        self.inner.send().await;

                        if self.inner.connection.is_closed() || self.inner.connection.is_draining() {
                            // Connection ending
                            break;
                        }

                        self.inner.flush().await;
                    }

                    // Outgoing traffic
                    Some(to_send) = self.out_rx.recv(), if should_send_out => {
                        match to_send {
                            OutgoingData::Stream(packet) => self.inner.send_packet(0, packet).await,
                            OutgoingData::Datagram(datagram) => self.inner.send_datagram(datagram).await,
                        }
                    }

                    // Cleanup
                    Some(termination) = self.term_rx.recv() => {
                        match termination {
                            Terminated::Incoming(id) => {
                                self.inner.incoming.transfers.remove(&id);
                            }

                            Terminated::Outgoing(id) => {
                                self.inner.outgoing.transfers.remove(&id);
                            }
                        }
                    }
                }
            }
        }

        if !self.inner.connection.is_draining() && !self.inner.connection.is_closed() {
            // Commands finished, close connection
            self.inner.connection.close(true, 0, b"Done").unwrap();
            self.inner.send().await;
        }

        loop {
            // TODO: Timeout
            self.inner.recv().await;
            self.inner.send().await;

            if self.inner.connection.is_closed() {
                println!("Connection closed");
                return;
            }
        }
    }

    async fn read_streams(&mut self) {
        for stream_id in self.inner.connection.readable() {
            let mut buf = self
                .stream_bufs
                .entry(stream_id)
                .or_insert_with(BytesMut::new);
            self.inner.read_stream(stream_id, &mut buf).await;
        }
    }
}

impl Inner {
    async fn send(&mut self) {
        const PACING_WINDOW: Duration = Duration::from_millis(1);

        loop {
            let info = match self.connection.send(&mut self.send_buf) {
                Ok(info) => info,
                Err(quiche::Error::Done) => return,
                err => err.unwrap(),
            };

            let (len, info) = info;

            if let Some(lapsed) = info.at.checked_duration_since(Instant::now()) {
                if lapsed > PACING_WINDOW {
                    tokio::time::sleep_until(info.at.into()).await;
                }
            }

            let mut to_send = &self.send_buf[..len];

            while !to_send.is_empty() {
                let sent = self.socket.send(&to_send).await.unwrap();
                to_send = &to_send[sent..];
            }
        }
    }

    async fn recv(&mut self) {
        let len = self.socket.recv(&mut self.recv_buf).await.unwrap();
        let mut to_read = &mut self.recv_buf[..len];

        while !to_read.is_empty() {
            //TODO: Backpressure
            let read = self.connection.recv(to_read, self.recv_info).unwrap();
            to_read = &mut to_read[read..];
        }
    }

    async fn read_stream(&mut self, stream_id: u64, mut buf: &mut BytesMut) {
        const BUFFER_BUMP: usize = 1024;

        let mut len = buf.len();
        buf.resize(len + BUFFER_BUMP, 0);

        while let Ok((read, _fin)) = self.connection.stream_recv(stream_id, &mut buf[len..]) {
            len += read;
            buf.resize(len + BUFFER_BUMP, 0);
        }

        buf.truncate(len);

        loop {
            let packet = match proto::frame(&mut buf) {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(e) => panic!("{}", e),
            };

            self.process_packet(stream_id, packet).await;
        }
    }

    async fn process_packet(&mut self, _stream_id: u64, packet: Packet) {
        let inner = packet.data.unwrap();
        match inner {
            Data::AcceptUpload(upload) => {
                let start = self.outgoing.pending.remove(&upload.id).unwrap();

                let (in_tx, in_rx) = mpsc::channel(32);
                self.outgoing.transfers.insert(upload.id, in_tx);
                start.send(in_rx).unwrap();
            }

            Data::AckEndRound(end) => {
                let transfer = self.outgoing.transfers.get_mut(&end.id).unwrap();
                transfer
                    .send(outgoing::IncomingPacket::AckEndRound(end))
                    .await
                    .unwrap();
            }

            Data::ControlUpdate(update) => {
                let transfer = self.outgoing.transfers.get_mut(&update.id).unwrap();
                transfer
                    .send(outgoing::IncomingPacket::ControlUpdate(update))
                    .await
                    .unwrap();
            }

            Data::AcceptDownload(download) => {
                let start = self.incoming.pending.remove(&download.id).unwrap();

                let (info_tx, info_rx) = mpsc::channel(32);
                let (piece_tx, piece_rx) = resizable::resizable(4);

                self.incoming
                    .transfers
                    .insert(download.id.clone(), IncomingHandle { info_tx, piece_tx });
                start
                    .send(DownloadStartHandle {
                        info_rx,
                        piece_rx,
                        info: download,
                    })
                    .unwrap();
            }

            Data::BlockInfo(info) => {
                let transfer = self.incoming.transfers.get_mut(&info.id).unwrap();
                transfer
                    .info_tx
                    .send(incoming::IncomingPacket::BlockInfo(info))
                    .await
                    .unwrap();
            }

            Data::EndRound(end) => {
                let transfer = self.incoming.transfers.get_mut(&end.id).unwrap();
                transfer
                    .info_tx
                    .send(incoming::IncomingPacket::EndRound(end))
                    .await
                    .unwrap();
            }

            Data::EndTransfer(end) => {
                let transfer = self.outgoing.transfers.get_mut(&end.id).unwrap();
                transfer
                    .send(outgoing::IncomingPacket::EndTransfer(end))
                    .await
                    .unwrap()
            }

            Data::Ping(_) => unimplemented!(),
            Data::StartBenchmark(_) => unimplemented!(),
            Data::StopBenchmark(_) => unimplemented!(),
            Data::RequestUpload(_) => unimplemented!(),
            Data::RequestDownload(_) => unimplemented!(),
        }
    }

    async fn read_dgrams(&mut self) {
        while let Ok(len) = self.connection.dgram_recv(&mut self.dgram_buf) {
            let datagram = Datagram::decode(&self.dgram_buf[..len]).unwrap();
            self.process_dgram(datagram).await;
        }
    }

    async fn process_dgram(&mut self, datagram: Datagram) {
        let data = datagram.data.unwrap();
        match data {
            datagram::Data::SendPiece(piece) => {
                let transfer = self.incoming.transfers.get_mut(&piece.id).unwrap();
                if let Err(TrySendError::Closed(_)) = transfer.piece_tx.try_send(piece) {
                    panic!("Transfer closed!");
                }
            }

            datagram::Data::BenchmarkPayload(_) => unimplemented!(),
        }
    }

    async fn send_packet(&mut self, stream_id: u64, packet: Packet) {
        let mut buf = BytesMut::new();
        packet.encode_length_delimited(&mut buf).unwrap();
        self.send_packet_buf(stream_id, buf).await;
    }

    async fn send_packet_buf(&mut self, stream_id: u64, mut buf: BytesMut) {
        while buf.remaining() > 0 {
            match self.connection.stream_send(stream_id, &buf, false) {
                Ok(0) => {
                    self.store_stream_buf(stream_id, buf);
                    return;
                }

                Ok(written) => buf.advance(written),

                Err(quiche::Error::Done) => {
                    self.store_stream_buf(stream_id, buf);
                    return;
                }

                Err(e) => {
                    panic!("Error while sending packet: {}", e);
                }
            }
        }

        self.send().await;
    }

    fn store_stream_buf(&mut self, stream_id: u64, buf: BytesMut) {
        if self.stream_slot.is_some() {
            panic!("Tried to send packet when slot was full");
        } else {
            self.stream_slot = Some((stream_id, buf));
        }
    }

    async fn send_datagram(&mut self, datagram: Datagram) {
        let mut buf = BytesMut::new();
        datagram.encode(&mut buf).unwrap();
        self.send_datagram_buf(buf).await;
    }

    async fn send_datagram_buf(&mut self, buf: BytesMut) {
        match self.connection.dgram_send(&buf) {
            Err(quiche::Error::Done) => {
                if self.dgram_slot.is_some() {
                    panic!("Tried to send datagram while slot was full")
                } else {
                    self.dgram_slot = Some(buf);
                    return;
                }
            }

            Err(e) => {
                panic!("Error while sending datagram: {}", e);
            }

            Ok(()) => {}
        }
        self.send().await;
    }

    async fn flush(&mut self) {
        if let Some(dgram) = self.dgram_slot.take() {
            self.send_datagram_buf(dgram).await;
        }

        if let Some((stream_id, packet)) = self.stream_slot.take() {
            self.send_packet_buf(stream_id, packet).await;
        }
    }
}

#[derive(Debug)]
enum Command {
    GetFile {
        local: String,
        remote: String,
        options: TransferOptions,
    },
    PutFile {
        local: String,
        remote: String,
        options: TransferOptions,
    },
}

#[derive(Debug)]
struct TransferOptions {
    integrity: bool,
}

impl TransferOptions {
    fn from_matches(matches: &ArgMatches) -> Self {
        let integrity = matches.value_of("integrity").unwrap().parse().unwrap();

        TransferOptions { integrity }
    }
}
