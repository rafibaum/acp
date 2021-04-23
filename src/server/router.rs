use crate::client::Client;
use anyhow::{Context, Result};
use bytes::BytesMut;
use quiche::ConnectionId;
use ring::rand::SecureRandom;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{channel, Receiver, Sender};

pub const DATAGRAM_SIZE: usize = u16::MAX as usize;

pub struct Router {
    socket: Arc<UdpSocket>,
    quic_config: quiche::Config,
    handles: HashMap<ConnectionId<'static>, Handle>,
    rng: ring::rand::SystemRandom,
    term_tx: Sender<ConnectionId<'static>>,
    term_rx: Receiver<ConnectionId<'static>>,
}

struct Handle {
    tx: Sender<BytesMut>,
}

impl Router {
    pub async fn new<A: ToSocketAddrs>(bind_addr: A, quic_config: quiche::Config) -> Result<Self> {
        let socket = Arc::new(
            UdpSocket::bind(bind_addr)
                .await
                .context("Could not bind server socket to address")?,
        );

        let (term_tx, term_rx) = channel(32);

        Ok(Router {
            socket,
            quic_config,
            handles: HashMap::new(),
            rng: ring::rand::SystemRandom::new(),
            term_tx,
            term_rx,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let mut recv_buf = BytesMut::new();
        recv_buf.resize(DATAGRAM_SIZE, 0);

        loop {
            tokio::select! {
                res = self.socket.recv_from(&mut recv_buf) => {
                    let (len, addr) = res.context("Failure while listening to server socket")?;
                    self.socket_data(len, addr, &mut recv_buf).await?;
                }

                Some(cid) = self.term_rx.recv() => {
                    self.handles.remove(&cid);
                }
            }
        }
    }

    async fn socket_data(
        &mut self,
        len: usize,
        addr: SocketAddr,
        recv_buf: &mut BytesMut,
    ) -> Result<()> {
        let header = match quiche::Header::from_slice(&mut recv_buf[..len], quiche::MAX_CONN_ID_LEN)
        {
            Ok(header) => header,
            Err(_) => {
                return Ok(());
            }
        };

        let mut buf = BytesMut::new();
        buf.resize(DATAGRAM_SIZE, 0);
        std::mem::swap(recv_buf, &mut buf);
        buf.resize(len, 0);

        if let Err(err) = match self.handles.get(&header.dcid) {
            Some(handle) => {
                match handle.tx.try_send(buf) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Closed(buf)) => {
                        // Client dropped, run a handshake instead
                        self.handshake(addr, &header, buf)
                    }
                    Err(TrySendError::Full(_)) => {
                        // Queue building, drop packet
                        Ok(())
                    }
                }
            }
            None => self.handshake(addr, &header, buf),
        } {}

        Ok(())
    }

    fn handshake(
        &mut self,
        addr: SocketAddr,
        header: &quiche::Header,
        buf: BytesMut,
    ) -> Result<()> {
        if header.ty != quiche::Type::Initial {
            return Ok(());
        }

        let mut scid = vec![0; quiche::MAX_CONN_ID_LEN];
        self.rng.fill(&mut scid).unwrap(); // Rng should always succeed
        let scid = ConnectionId::from_vec(scid);

        let (tx, rx) = channel(32);
        tx.try_send(buf).unwrap(); // Receiver is in scope

        let connection = quiche::accept(&scid, None, &mut self.quic_config)
            .context("Failed to initialise QUIC connection to client")?;
        let handle = Handle::new(tx);
        self.handles.insert(scid.clone(), handle);

        let client = Client::new(
            self.socket.clone(),
            addr,
            connection,
            rx,
            scid,
            self.term_tx.clone(),
        );

        tokio::spawn(async move {
            async move {
                if let Err(e) = client.run().await {
                    eprintln!("Client error: {}", e);
                }
            }
            .await
        });

        Ok(())
    }
}

impl Handle {
    fn new(tx: Sender<BytesMut>) -> Self {
        Handle { tx }
    }
}
