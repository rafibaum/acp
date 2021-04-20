//! ACP client.
//!
//! A client for `acp` servers.

#![warn(missing_docs)]

use anyhow::Result;
use bytes::{Buf, BytesMut};
use libacp::outgoing::Outgoing;
use libacp::proto::packet::Data;
use libacp::proto::{Datagram, Packet, RequestUpload};
use libacp::{outgoing, proto};
use prost::Message;
use quiche::{CongestionControlAlgorithm, Connection, ConnectionId};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;

/// Client's main function.
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let local = &args[1];
    let remote = &args[2];

    let sock = UdpSocket::bind("127.0.0.1:55281").await.unwrap();
    sock.connect("127.0.0.1:55280").await.unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_application_protos(b"\x07acp/0.1").unwrap();
    config.verify_peer(false);
    config.set_max_recv_udp_payload_size(65535);
    config.set_max_send_udp_payload_size(65535);
    config.set_initial_max_data(1000000);
    config.set_initial_max_streams_bidi(1);
    config.set_initial_max_streams_uni(1);
    config.set_initial_max_stream_data_bidi_local(1000000);
    config.set_initial_max_stream_data_bidi_remote(1000000);
    config.set_initial_max_stream_data_uni(1000000);
    config.set_max_idle_timeout(30 * 1000);
    config.enable_dgram(true, 512, 512);
    config.set_cc_algorithm(CongestionControlAlgorithm::BBR);

    let rng = ring::rand::SystemRandom::new();
    let mut scid = vec![0; quiche::MAX_CONN_ID_LEN];
    rng.fill(&mut scid).unwrap();
    let scid = ConnectionId::from_vec(scid);

    let conn = quiche::connect(None, &scid, &mut config).unwrap();

    let (tx, rx) = mpsc::channel(32);
    let client = Client::new(conn, sock, rx, rng);

    tx.send(Command::PutFile {
        local: local.clone(),
        remote: remote.clone(),
    })
    .await
    .unwrap();

    // Set up command input
    std::mem::drop(tx);

    client.run().await;

    Ok(())
}

struct Client {
    inner: Inner,
    out_rx: Receiver<outgoing::OutgoingPacket>,
    stream_bufs: HashMap<u64, BytesMut>,
}

struct Inner {
    connection: Pin<Box<Connection>>,
    socket: UdpSocket,
    send_buf: BytesMut,
    recv_buf: BytesMut,
    commands: Receiver<Command>,
    rng: SystemRandom,
    transfers: HashMap<Vec<u8>, Sender<outgoing::IncomingPacket>>,
    out_tx: Sender<outgoing::OutgoingPacket>,
    dgram_buf: BytesMut,
    pending_transfers: HashMap<Vec<u8>, oneshot::Sender<Receiver<outgoing::IncomingPacket>>>,
}

impl Client {
    pub fn new(
        connection: Pin<Box<Connection>>,
        socket: UdpSocket,
        commands: Receiver<Command>,
        rng: SystemRandom,
    ) -> Self {
        let mut send_buf = BytesMut::with_capacity(65535);
        send_buf.resize(65535, 0);

        let mut recv_buf = BytesMut::with_capacity(65535);
        recv_buf.resize(65535, 0);

        let mut dgram_buf = BytesMut::with_capacity(65535);
        dgram_buf.resize(65535, 0);

        let (out_tx, out_rx) = mpsc::channel(32);

        Client {
            inner: Inner {
                connection,
                socket,
                send_buf,
                recv_buf,
                commands,
                rng,
                transfers: HashMap::new(),
                dgram_buf,
                out_tx,
                pending_transfers: HashMap::new(),
            },
            stream_bufs: HashMap::new(),
            out_rx,
        }
    }

    pub async fn run(mut self) {
        // Initialise connection
        self.inner.send().await;
        loop {
            self.inner.recv().await;
            self.inner.send().await;

            if self.inner.connection.is_closed() {
                //TODO: Handle
                panic!("Connection was closed before it got established");
            }

            if self.inner.connection.is_established() && !self.inner.connection.is_draining() {
                break;
            }
        }

        // Initialised, start processing commands
        while let Some(command) = self.inner.commands.recv().await {
            let handle = match command {
                Command::PutFile { local, remote } => {
                    let file = File::open(local).await.expect("File not found");

                    let mut id = vec![0; 16];
                    self.inner.rng.fill(&mut id).unwrap();

                    let (start_tx, start_rx) = oneshot::channel();
                    self.inner.pending_transfers.insert(id.clone(), start_tx);

                    let packet = Packet::new(Data::RequestUpload(RequestUpload {
                        id: id.clone(),
                        filename: remote.into_bytes(),
                        size: file.metadata().await.unwrap().len(),
                        block_size: 200,
                        piece_size: 16000,
                    }));
                    self.inner.send_packet(0, packet).await;

                    let out_tx = self.inner.out_tx.clone();

                    tokio::spawn(async {
                        let in_rx = start_rx.await.unwrap();
                        let outgoing = Outgoing::new(id, file, 200, 16000, out_tx, in_rx);
                        outgoing.run().await
                    })
                }
            };

            tokio::pin!(handle);

            // Process network traffic until command finished
            loop {
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

                        self.read_streams().await;
                        self.inner.read_dgrams();
                    }

                    // Outgoing traffic
                    Some(to_send) = self.out_rx.recv() => {
                        match to_send {
                            outgoing::OutgoingPacket::Stream(packet) => self.inner.send_packet(0, packet).await,
                            outgoing::OutgoingPacket::Datagram(datagram) => self.inner.send_datagram(datagram).await,
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
        const PACING_WINDOW: Duration = Duration::from_millis(10);

        loop {
            let info = match self.connection.send_with_info(&mut self.send_buf) {
                Ok(info) => info,
                Err(quiche::Error::Done) => return,
                err => err.unwrap(),
            };

            let (len, info) = info;

            if let Some(lapsed) = info.send_time.checked_duration_since(Instant::now()) {
                if lapsed > PACING_WINDOW {
                    tokio::time::sleep_until(info.send_time.into()).await;
                }
            }

            let mut to_send = &self.send_buf[..len];

            while !to_send.is_empty() {
                let sent = self.socket.send(&to_send).await.unwrap();
                //TODO: Ensure progress is made
                if sent == 0 {
                    panic!("Nothing sent");
                }
                to_send = &to_send[sent..];
            }
        }
    }

    async fn recv(&mut self) {
        // Splitting borrows for select
        let recv_buf = &mut self.recv_buf;
        let connection = &mut self.connection;

        tokio::select! {
            // Received bytes
            len = self.socket.recv(recv_buf) => {
                let len = len.unwrap();
                let mut to_read = &mut self.recv_buf[..len];

                while !to_read.is_empty() {
                    //TODO: Backpressure
                    let read = self.connection.recv(to_read).unwrap();
                    to_read = &mut to_read[read..];
                }
            }

            // Timeout
            true = async {
                let timeout = match connection.timeout() {
                    Some(timeout) => timeout,
                    None => return false, // Disable branch, wait for packets
                };

                tokio::time::sleep(timeout).await;
                true // Timeout occurred
            } => {
                self.connection.on_timeout();
            }
        }
    }

    async fn read_stream(&mut self, stream_id: u64, mut buf: &mut BytesMut) {
        const BUFFER_BUMP: usize = 1024;

        let mut len = buf.len();
        buf.resize(len + BUFFER_BUMP, 0);

        while let Ok((read, _fin)) = self.connection.stream_recv(stream_id, &mut buf[len..]) {
            //TODO: Handle fin
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
                let start = self.pending_transfers.remove(&upload.id).unwrap();

                let (in_tx, in_rx) = mpsc::channel(32);
                self.transfers.insert(upload.id, in_tx);
                start.send(in_rx).unwrap();
            }

            Data::AckEndRound(end) => {
                let transfer = self.transfers.get_mut(&end.id).unwrap();
                transfer
                    .send(outgoing::IncomingPacket::AckEndRound(end))
                    .await
                    .unwrap();
            }

            Data::ControlUpdate(update) => {
                let transfer = self.transfers.get_mut(&update.id).unwrap();
                transfer
                    .send(outgoing::IncomingPacket::ControlUpdate(update))
                    .await
                    .unwrap();
            }

            _ => unimplemented!(),
        }
    }

    fn read_dgrams(&mut self) {
        while let Ok(len) = self.connection.dgram_recv(&mut self.dgram_buf) {
            let datagram = Datagram::decode(&self.dgram_buf[..len]).unwrap();
            self.process_dgram(datagram);
        }
    }

    fn process_dgram(&mut self, _datagram: Datagram) {
        unimplemented!()
    }

    async fn send_packet(&mut self, stream_id: u64, packet: Packet) {
        let mut buf = BytesMut::new();
        packet.encode_length_delimited(&mut buf).unwrap();
        while buf.remaining() > 0 {
            let written = self.connection.stream_send(stream_id, &buf, false).unwrap();

            if written == 0 {
                panic!("No progress made");
            }

            buf.advance(written);
        }

        self.send().await;
    }

    async fn send_datagram(&mut self, datagram: Datagram) {
        let mut buf = BytesMut::new();
        datagram.encode(&mut buf).unwrap();
        self.connection.dgram_send(&buf).unwrap();
        self.send().await;
    }
}

#[derive(Debug)]
enum Command {
    PutFile { local: String, remote: String },
}
