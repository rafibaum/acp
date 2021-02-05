//! ACP client.
//!
//! A client for `acp` servers.

#![warn(missing_docs)]

use libacp::proto::packet::Data;
use libacp::proto::{Handshake, Packet, Terminate};
use prost::Message;
use tokio::net::UdpSocket;

/// Client's main function.
#[tokio::main]
async fn main() {
    let sock = UdpSocket::bind("127.0.0.1:55281").await.unwrap();
    sock.connect("127.0.0.1:55280").await.unwrap();

    // Send handshake
    let handshake = Packet {
        data: Some(Data::Handshake(Handshake {
            name: String::from("Rafi"),
        })),
    };

    let mut buf = Vec::new();
    handshake.encode_length_delimited(&mut buf).unwrap();
    sock.send(&buf).await.unwrap();

    // Send terminate
    let termination = Packet {
        data: Some(Data::Terminate(Terminate {})),
    };

    buf.clear();
    termination.encode_length_delimited(&mut buf).unwrap();
    sock.send(&buf).await.unwrap();
}
