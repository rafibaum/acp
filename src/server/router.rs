//! Manages the network socket and routes inbound packets to clients.
//!
//! The router is responsible for managing the inbound portion of communication. As UDP is
//! connectionless, this abstraction is necessary to route UDP datagrams to their respective clients
//! and set up new clients as handshakes are received. The router is also responsible for top-level
//! client error handling.

use crate::client::Client;
use crate::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;

/// Router controller.
pub(crate) struct Router {
    /// Socket this server listens on.
    socket: UdpSocket,
    /// Map of clients from their addresses to their inbound receiver channels.
    ///
    /// This is the main data structure used to map inbound datagrams to their respective clients.
    /// It's wrapped in a [`Mutex`] as clients need access to prune themselves from the map once
    /// they're terminated.
    // TODO: Replace mutex with message passing
    clients: Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Vec<u8>>>>>,
}

impl Router {
    /// Binds the socket and, if successful, constructs a new router.
    ///
    /// # Errors
    /// If the address passed in fails to bind.
    pub async fn new<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;

        Ok(Router {
            socket,
            clients: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Main loop of the router.
    ///
    /// Takes incoming packets and maps them to the appropriate clients, setting up new clients
    /// where necessary.
    pub async fn process(&mut self) -> Result<()> {
        loop {
            // Construct a vec that can handle the maximum size of a UDP datagram. This will be
            // passed around amongst tasks so heap allocating makes sense (and this would blow out
            // the stack at this size anyways).
            let mut buf = vec![0; u16::MAX as usize];

            let (_, addr) = self.socket.recv_from(&mut buf).await?;

            // Find client to route to
            let clients = self.clients.lock()?;
            let handle = clients.get(&addr);

            match handle {
                // Client may have been found
                Some(handle) => {
                    if let Err(e) = handle.send(buf) {
                        // Old handle has no open clients, set up new connection instead
                        self.handshake(clients, addr, e.0);
                    }
                }

                // No client, start handshake
                None => {
                    self.handshake(clients, addr, buf);
                }
            }
        }
    }

    /// Sets up a new client.
    fn handshake(
        &self,
        mut clients: MutexGuard<'_, HashMap<SocketAddr, UnboundedSender<Vec<u8>>>>,
        addr: SocketAddr,
        buf: Vec<u8>,
    ) {
        let (client_tx, client_rx) = mpsc::unbounded_channel();
        // We should never get this error but I don't want unwrap in here at all
        client_tx
            .send(buf)
            .expect("Handshake receiver dropped out of scope");
        clients.insert(addr, client_tx);

        // Drop the mutex lock, create HashMap reference for client handle
        let clients = self.clients.clone();

        // Spawn the client and manage top-level errors
        tokio::spawn(async move {
            let mut client = Client::new(client_rx, clients, addr);

            if let Err(e) = client.process().await {
                println!("Client at {} encountered an error: {}", client.addr(), e);
                if let Err(poisoned) = client.terminate() {
                    println!("Couldn't terminate client gracefully: {}", poisoned);
                }
            }
        });
    }
}
