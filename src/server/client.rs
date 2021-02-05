//! Controls communication between the server and a single client.
//!
//! The client is responsible for receiving packets from the [router](crate::router) through a
//! channel and processing them as necessary. The client may fully process packets within its own
//! task or spawn new tasks as necessary, taking responsibility for the lifecycle of spawned tasks.

use crate::{AcpServerError, Result};
use libacp::proto::packet::Data;
use libacp::proto::Packet;
use prost::alloc::fmt::Formatter;
use prost::Message;
use std::collections::HashMap;
use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Client controller.
pub(crate) struct Client {
    /// Channel of bytes received from this client.
    input: UnboundedReceiver<Vec<u8>>,
    /// List of clients from the [router](crate::router). Used to remove this client from the router
    /// on termination.
    clients: Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Vec<u8>>>>>,
    /// Address of the client.
    // TODO: Replace with a session identifier
    addr: SocketAddr,
    /// Current state of the client's connection.
    state: ClientState,
}

impl Client {
    /// Constructs a new client.
    pub fn new(
        client_rx: UnboundedReceiver<Vec<u8>>,
        clients: Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Vec<u8>>>>>,
        addr: SocketAddr,
    ) -> Self {
        Client {
            input: client_rx,
            clients,
            addr,
            state: ClientState::Unauthorised,
        }
    }

    /// Returns the IP address of the client.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The main loop of the client task.
    pub async fn process(&mut self) -> Result<()> {
        loop {
            let data = match self.input.recv().await {
                Some(data) => data,
                None => {
                    // TODO: Terminate client connection
                    // Right now we don't have client-side termination so we should just close out
                    self.terminate()?;
                    break;
                }
            };

            let packet: Packet = Packet::decode_length_delimited(&data[..])?;

            // Dispatch packet to different handler depending on client state
            match self.state {
                ClientState::Unauthorised => self.unauthorised(packet)?,
                ClientState::Authorised => self.authorised(packet)?,
                ClientState::Terminated => {
                    // We should never come across this state while processing a packet
                    return Err(AcpServerError::IllegalClientState(self.state));
                }
            }

            // Stop processing packets if handlers terminated our connection
            if self.state == ClientState::Terminated {
                break;
            }
        }

        Ok(())
    }

    /// Handles incoming packets while the client is unauthorised.
    fn unauthorised(&mut self, packet: Packet) -> Result<()> {
        let data = match &packet.data {
            Some(data) => data,
            None => return Err(AcpServerError::MalformedPacket(self.state, packet)),
        };

        // Process payload
        match data {
            Data::Handshake(handshake) => {
                println!("Handshake from: {}", handshake.name);
                self.state = ClientState::Authorised;
            }
            _ => return Err(AcpServerError::UnexpectedPacket(self.state, packet)),
        }

        Ok(())
    }

    /// Handles incoming packets while the client is authorised.
    fn authorised(&mut self, packet: Packet) -> Result<()> {
        let data = match &packet.data {
            Some(data) => data,
            None => return Err(AcpServerError::MalformedPacket(self.state, packet)),
        };

        // Process payload
        match data {
            Data::Terminate(_) => {
                println!("Terminating");
                self.terminate()?;
            }
            _ => println!("Unexpected packet encountered while authorised"),
        }

        Ok(())
    }

    /// Clean up the client's connection.
    pub(crate) fn terminate(&mut self) -> Result<()> {
        // TODO: Implement explicit client termination signal
        let mut clients = self.clients.lock()?;
        clients.remove(&self.addr);
        self.state = ClientState::Terminated;
        Ok(())
    }
}

/// Represents the current state of the client's connection.
///
/// Packets may need to be processed differently depending on if a user is authorised or not, so
/// this enum is used to track state for the client.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum ClientState {
    /// Awaiting client handshake.
    Unauthorised,
    /// Client fully connected.
    Authorised,
    /// Connection with client has been terminated.
    Terminated,
}

impl Display for ClientState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ClientState::Unauthorised => "Unauthorised",
            ClientState::Authorised => "Authorised",
            ClientState::Terminated => "Terminated",
        };

        write!(f, "{}", name)
    }
}
