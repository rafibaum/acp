//! ACP client.
//!
//! A client for `acp` servers.

#![warn(missing_docs)]

use anyhow::Result;
use bytes::{Buf, BytesMut};
use libacp::proto;
use libacp::proto::packet::Data;
use libacp::proto::{Packet, Ping};
use prost::Message;
use quiche::ConnectionId;
use ring::rand::SecureRandom;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Client's main function.
#[tokio::main]
async fn main() -> Result<()> {
    let sock = UdpSocket::bind("127.0.0.1:55281").await?;
    sock.connect("127.0.0.1:55280").await?;

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    config.set_application_protos(b"\x07acp/0.1")?;
    config.verify_peer(false);
    config.set_max_recv_udp_payload_size(65535);
    config.set_max_send_udp_payload_size(65535);
    config.set_initial_max_data(1000000);
    config.set_initial_max_streams_bidi(1);
    config.set_initial_max_streams_uni(1);
    config.set_initial_max_stream_data_bidi_local(1000000);
    config.set_initial_max_stream_data_bidi_remote(1000000);
    config.set_initial_max_stream_data_uni(1000000);

    let rng = ring::rand::SystemRandom::new();
    let mut scid = vec![0; quiche::MAX_CONN_ID_LEN];
    rng.fill(&mut scid)?;
    let scid = ConnectionId::from_vec(scid);

    let mut conn = quiche::connect(None, &scid, &mut config)?;
    send(&mut conn, &sock).await?;

    loop {
        recv(&mut conn, &sock).await?;
        send(&mut conn, &sock).await?;
        if conn.is_established() {
            break;
        }
    }

    let packet = Packet::new(Data::Ping(Ping {
        data: String::from("Rafi"),
    }));
    send_packet(&mut conn, &sock, 0, packet).await?;

    loop {
        recv(&mut conn, &sock).await?;
        send(&mut conn, &sock).await?;

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

                while let Some(packet) = proto::frame(&mut buf)? {
                    // TODO: Handle
                    match packet.data.unwrap() {
                        Data::Ping(ping) => {
                            println!("Got ping: {}", ping.data);
                            conn.close(true, 0, b"DONE")?;
                            send(&mut conn, &sock).await?;
                        }
                        _ => panic!("Unexpected packet"),
                    }
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
            let written = socket.send(&send_buf[sent..len]).await?;
            sent += written;
        }
    }
}

async fn send_packet(
    connection: &mut quiche::Connection,
    socket: &UdpSocket,
    stream_id: u64,
    packet: Packet,
) -> Result<()> {
    let mut buf = BytesMut::new();
    packet.encode_length_delimited(&mut buf)?;
    println!("Sending: {:?}", &buf);
    while buf.remaining() > 0 {
        buf.advance(connection.stream_send(stream_id, &buf[..], false)?);
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
    }?;

    match connection.recv(&mut recv_buf[..len]) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
