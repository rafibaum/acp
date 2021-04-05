//! ACP client.
//!
//! A client for `acp` servers.

#![warn(missing_docs)]

use anyhow::Result;
use bytes::{Buf, BytesMut};
use libacp::outgoing::{IncomingPacket, Outgoing, OutgoingPacket};
use libacp::proto;
use libacp::proto::packet::Data;
use libacp::proto::{datagram, packet, BenchmarkPayload, StartTransfer, StopBenchmark};
use libacp::proto::{Datagram, Packet, Ping};
use prost::Message;
use quiche::ConnectionId;
use ring::rand::SecureRandom;
use std::time::Instant;
use tokio::fs::File;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Client's main function.
#[tokio::main]
async fn main() -> Result<()> {
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

    let rng = ring::rand::SystemRandom::new();
    let mut scid = vec![0; quiche::MAX_CONN_ID_LEN];
    rng.fill(&mut scid).unwrap();
    let scid = ConnectionId::from_vec(scid);

    let mut conn = quiche::connect(None, &scid, &mut config).unwrap();
    send(&mut conn, &sock).await.unwrap();

    loop {
        recv(&mut conn, &sock).await.unwrap();
        send(&mut conn, &sock).await.unwrap();
        if conn.is_established() {
            break;
        }
    }

    let packet = Packet::new(packet::Data::Ping(Ping {
        data: String::from("Rafi"),
    }));
    send_packet(&mut conn, &sock, 0, packet).await.unwrap();

    let file = File::open("input.tar.gz").await.unwrap();
    let filesize = file.metadata().await.unwrap().len();

    let packet = Packet::new(packet::Data::StartTransfer(StartTransfer {
        id: vec![72, 82],
        filename: Vec::from("output.tar.gz"),
        size: filesize,
        block_size: 200,
        piece_size: 16000,
    }));
    send_packet(&mut conn, &sock, 0, packet).await.unwrap();
    std::mem::drop(file);

    let mut benchmark_enabled = None;
    let packet = Datagram::new(datagram::Data::BenchmarkPayload(BenchmarkPayload {
        payload: vec![0; 15000],
    }));
    let mut benchmark_payload = BytesMut::new();
    packet.encode(&mut benchmark_payload).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut update_tx = None;

    loop {
        tokio::select! {
            result = recv(&mut conn, &sock) => {
                result.unwrap();
            }

            Some(outgoing) = rx.recv() => {
                send_outgoing(&mut conn, &sock, outgoing).await.unwrap();
            }
        }
        send(&mut conn, &sock).await.unwrap();

        if conn.is_closed() {
            break;
        }

        const QUEUE_BUMP_SIZE: usize = 1024;
        if conn.is_established() && !conn.is_draining() {
            for stream_id in conn.readable() {
                let mut buf = BytesMut::new();
                let mut len = buf.len();
                buf.resize(len + QUEUE_BUMP_SIZE, 0);

                // TODO: Handle fin
                while let Ok((read, _fin)) = conn.stream_recv(stream_id, &mut buf[len..]) {
                    len += read;
                    buf.resize(len + QUEUE_BUMP_SIZE, 0);
                }

                buf.resize(len, 0);

                while let Some(packet) = proto::frame(&mut buf).unwrap() {
                    // TODO: Handle
                    match packet.data.unwrap() {
                        packet::Data::Ping(ping) => {
                            println!("Got ping: {}", ping.data);
                        }
                        packet::Data::StartBenchmark(_) => {
                            println!("Starting benchmarking");
                            benchmark_enabled = Some(Instant::now());
                        }
                        packet::Data::StopBenchmark(_) => {
                            benchmark_enabled = None;
                            conn.dgram_purge_outgoing(&|_: &[u8]| -> bool { true });
                        }
                        packet::Data::AcceptTransfer(accept) => {
                            let file = File::open("input.tar.gz").await.unwrap();

                            let (temp_update_tx, update_rx) =
                                tokio::sync::mpsc::unbounded_channel();
                            let outgoing =
                                Outgoing::new(accept.id, file, 200, 16000, tx.clone(), update_rx);
                            update_tx = Some(temp_update_tx);

                            tokio::spawn(async move {
                                outgoing.run().await;
                            });
                        }

                        Data::ControlUpdate(update) => {
                            if let Some(tx) = update_tx.as_mut() {
                                if tx.send(IncomingPacket::ControlUpdate(update)).is_err() {
                                    // No more file transfer
                                }
                            }
                        }

                        Data::AckEndRound(ack) => {
                            if let Some(tx) = update_tx.as_mut() {
                                if tx.send(IncomingPacket::AckEndRound(ack)).is_err() {
                                    // No more file transfer
                                }
                            }
                        }

                        packet::Data::StartTransfer(_) => unimplemented!(),
                        packet::Data::BlockInfo(_) => unimplemented!(),
                        packet::Data::EndRound(_) => unimplemented!(),
                    }
                }
            }

            if let Some(epoch) = benchmark_enabled {
                if Instant::now().duration_since(epoch).as_secs() >= 10 {
                    benchmark_enabled = None;
                    send_packet(
                        &mut conn,
                        &sock,
                        0,
                        Packet::new(packet::Data::StopBenchmark(StopBenchmark {})),
                    )
                    .await
                    .unwrap();
                } else {
                    if let Err(e) = conn.dgram_send(&benchmark_payload) {
                        panic!("Error during benchmarking: {:?}", e);
                    }
                    send(&mut conn, &sock).await.unwrap();
                }
            }
        }
    }

    Ok(())
}

async fn send(connection: &mut quiche::Connection, socket: &UdpSocket) -> Result<()> {
    let mut send_buf = BytesMut::new();
    send_buf.resize(65535, 0);

    loop {
        let len = match connection.send(&mut send_buf) {
            Ok(len) => len,
            Err(quiche::Error::Done) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let mut sent = 0;
        while sent < len {
            let written = socket.send(&send_buf[sent..len]).await.unwrap();
            sent += written;
        }
    }
}

async fn send_datagram(
    connection: &mut quiche::Connection,
    socket: &UdpSocket,
    datagram: Datagram,
) -> Result<()> {
    let mut buf = BytesMut::new();
    datagram.encode(&mut buf).unwrap();
    connection.dgram_send(&buf).unwrap();

    send(connection, socket).await
}

async fn send_outgoing(
    connection: &mut quiche::Connection,
    socket: &UdpSocket,
    outgoing: OutgoingPacket,
) -> Result<()> {
    match outgoing {
        OutgoingPacket::Stream(packet) => send_packet(connection, socket, 0, packet).await,
        OutgoingPacket::Datagram(datagram) => send_datagram(connection, socket, datagram).await,
    }
}

async fn send_packet(
    connection: &mut quiche::Connection,
    socket: &UdpSocket,
    stream_id: u64,
    packet: Packet,
) -> Result<()> {
    let mut buf = BytesMut::new();
    packet.encode_length_delimited(&mut buf).unwrap();
    while buf.remaining() > 0 {
        buf.advance(connection.stream_send(stream_id, &buf[..], false).unwrap());
    }

    send(connection, socket).await
}

async fn recv(connection: &mut quiche::Connection, socket: &UdpSocket) -> Result<()> {
    let mut recv_buf = BytesMut::new();
    recv_buf.resize(65535, 0);
    let socket_future = socket.recv(&mut recv_buf);
    let len = match connection.timeout() {
        Some(duration) => match timeout(duration, socket_future).await {
            Ok(len) => len,
            Err(_) => {
                connection.on_timeout();
                return Ok(());
            }
        },
        None => socket_future.await,
    }
    .unwrap();

    match connection.recv(&mut recv_buf[..len]) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
