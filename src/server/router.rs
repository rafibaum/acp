use crate::client::Client;
use anyhow::{Context, Result};
use bytes::BytesMut;
use quiche::ConnectionId;
use ring::rand::SecureRandom;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::{debug, info, trace, trace_span, Instrument};

pub const DATAGRAM_SIZE: usize = u16::MAX as usize;

pub struct Router {
    socket: Arc<UdpSocket>,
    quic_config: quiche::Config,
    handles: HashMap<ConnectionId<'static>, Handle>,
    rng: ring::rand::SystemRandom,
    recv_buf: BytesMut,
}

struct Handle {
    tx: UnboundedSender<BytesMut>,
}

impl Router {
    pub async fn new<A: ToSocketAddrs>(bind_addr: A, quic_config: quiche::Config) -> Result<Self> {
        let socket = Arc::new(
            UdpSocket::bind(bind_addr)
                .await
                .context("Could not bind server socket to address")?,
        );

        let mut recv_buf = BytesMut::new();
        recv_buf.resize(DATAGRAM_SIZE, 0);

        Ok(Router {
            socket,
            quic_config,
            handles: HashMap::new(),
            rng: ring::rand::SystemRandom::new(),
            recv_buf,
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn run(mut self) -> Result<()> {
        loop {
            self.poll_socket().await?
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn poll_socket(&mut self) -> Result<()> {
        let (len, addr) = self
            .socket
            .recv_from(&mut self.recv_buf)
            .await
            .context("Failure while listening to server socket")?;

        trace!(%addr, len, "received datagram");

        let header =
            match quiche::Header::from_slice(&mut self.recv_buf[..len], quiche::MAX_CONN_ID_LEN) {
                Ok(header) => header,
                Err(_) => {
                    debug!("received packet without QUIC header");
                    return Ok(());
                }
            };

        let mut buf = BytesMut::new();
        buf.resize(DATAGRAM_SIZE, 0);
        std::mem::swap(&mut self.recv_buf, &mut buf);
        buf.resize(len, 0);

        if let Err(_e) = match self.handles.get(&header.dcid) {
            Some(handle) => {
                match handle.tx.send(buf) {
                    Ok(()) => {
                        trace!(size = len, "datagram passed to client handle");
                        Ok(())
                    }
                    Err(e) => {
                        // Client dropped, run a handshake instead
                        self.handshake(addr, &header, e.0)
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
        let span = trace_span!("handshake", ty = ?header.ty, version = header.version, scid = ?header.scid.as_ref());
        let _enter = span.enter();

        if header.ty != quiche::Type::Initial {
            return Ok(());
        }

        let mut scid = vec![0; quiche::MAX_CONN_ID_LEN];
        self.rng.fill(&mut scid).unwrap(); // Rng should always succeed
        let scid = ConnectionId::from_vec(scid);

        let (tx, rx) = unbounded_channel();
        tx.send(buf).unwrap(); // Receiver is in scope

        let connection = quiche::accept(&scid, None, &mut self.quic_config)
            .context("Failed to initialise QUIC connection to client")?;
        let handle = Handle::new(tx);
        self.handles.insert(scid, handle);

        let client = Client::new(self.socket.clone(), addr, connection, rx);

        tokio::spawn(async move {
            let span = trace_span!("client");
            async move {
                info!("starting client");
                if let Err(e) = client.run().await {
                    eprintln!("Client error: {}", e);
                }
                info!("terminating client");
            }
            .instrument(span)
            .await
        });

        Ok(())
    }
}

impl Handle {
    fn new(tx: UnboundedSender<BytesMut>) -> Self {
        Handle { tx }
    }
}
