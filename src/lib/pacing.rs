use bytes::{Buf, BytesMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{Receiver, Sender};

pub struct Pacer {
    out: Arc<UdpSocket>,
    addr: SocketAddr,
    rx: Receiver<(BytesMut, Instant)>,
}

pub fn spawn_pacer(out: Arc<UdpSocket>, addr: SocketAddr) -> Sender<(BytesMut, Instant)> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let pacer = Pacer::new(out, addr, rx);
    tokio::task::spawn(pacer.run());
    tx
}

impl Pacer {
    pub fn new(out: Arc<UdpSocket>, addr: SocketAddr, rx: Receiver<(BytesMut, Instant)>) -> Self {
        Pacer { out, addr, rx }
    }

    pub async fn run(mut self) {
        loop {
            let packet = match self.rx.recv().await {
                Some(packet) => packet,
                None => {
                    // Channel dropped
                    return;
                }
            };
            let (mut buf, send_time) = packet;

            if let Some(sleep_time) = send_time.checked_duration_since(Instant::now()) {
                // We need to sleep a bit
                spin_sleep::sleep(sleep_time);
            }

            while !buf.is_empty() {
                let sent = self.out.send_to(&buf, self.addr).await.unwrap();
                buf.advance(sent);
            }
        }
    }
}
