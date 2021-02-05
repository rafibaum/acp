//! ACP server.
//!
//! Implements the `acp` protocol for fast file transfers.

#![warn(missing_docs)]

mod client;
mod router;

use crate::client::ClientState;
use crate::router::Router;
use libacp::proto::Packet;
use libacp::AcpError;
use prost::alloc::fmt::Formatter;
use std::error::Error;
use std::fmt::Display;

/// Server's main function. Starts the router and manages top-level error handling.
#[tokio::main]
async fn main() {
    // TODO: Implement configurable bind point
    let mut router = match Router::new("127.0.0.1:55280").await {
        Err(e) => {
            let err: AcpServerError = e.into();
            println!("{}", err);
            return;
        }
        Ok(server) => server,
    };

    if let Err(e) = router.process().await {
        println!("{}", e);
    }
}

/// The return type used for ACP server error handling.
pub type Result<T> = std::result::Result<T, AcpServerError>;

/// Errors which can occur during ACP server execution.
#[derive(Debug)]
pub enum AcpServerError {
    /// Wrapper type for errors from [`libacp`].
    AcpError(AcpError),
    /// Client has entered a state which it should not be in during execution.
    ///
    /// i.e. the client is in the terminated state but it tries to process a new packet.
    IllegalClientState(ClientState),
    /// A packet does not match the logical format we would expect it to.
    ///
    /// This can happen if a packet is missing a field which is required. Note that this does not
    /// mean there are errors in how the packet is encoded, those would be represented by
    /// [`PacketDecodeError`](libacp::AcpError::PacketDecodeError).
    MalformedPacket(ClientState, Packet),
    /// Client has received a packet it didn't expect to in the current state.
    ///
    /// Depending on the client's state, some client requests cannot be processed and this error
    /// is thrown instead.
    UnexpectedPacket(ClientState, Packet),
}

impl Display for AcpServerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpServerError::AcpError(err) => {
                write!(f, "{}", err)
            }
            AcpServerError::IllegalClientState(state) => {
                write!(f, "Client illegally in state {}", state)
            }
            AcpServerError::MalformedPacket(state, packet) => {
                write!(
                    f,
                    "Received malformed packet while in state {}: {:?}",
                    state, packet
                )
            }
            AcpServerError::UnexpectedPacket(state, packet) => {
                write!(
                    f,
                    "Received unexpected packet while in state {}: {:?}",
                    state, packet
                )
            }
        }
    }
}

impl Error for AcpServerError {}

impl<T: Into<AcpError>> From<T> for AcpServerError {
    fn from(err: T) -> Self {
        AcpServerError::AcpError(err.into())
    }
}
