//! ACP server.
//!
//! Implements the `acp` protocol for fast file transfers.

#![warn(missing_docs)]

mod client;
mod router;

use crate::client::ClientState;
use crate::router::Router;
use anyhow::Result;
use libacp::proto::Packet;
use thiserror::Error;

/// Server's main function. Starts the router and manages top-level error handling.
#[tokio::main]
async fn main() -> Result<()> {
    // TODO: Implement configurable bind point
    let mut router = Router::new("127.0.0.1:55280").await?;
    router.process().await?;
    Ok(())
}

/// Errors thrown during execution of the acp server.
#[derive(Debug, Error)]
pub enum AcpServerError {
    /// Thrown when a server enters a state that it should not have at that point in execution.
    /// This error should never be thrown in normal execution.
    #[error("client improperly entered state: `{0}`")]
    IllegalClientState(ClientState),
    /// Thrown when a server encounters a packet which has been improperly structured or corrupted.
    #[error("received a malformed packet while in state `{0}`")]
    MalformedPacket(ClientState, Packet),
    /// Thrown when a server encounters a packet it didn't expect to at that point in execution.
    #[error("received an unexpected packet in state `{0}`")]
    UnexpectedPacket(ClientState, Packet),
}
