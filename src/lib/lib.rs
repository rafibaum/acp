//! Common functionality for clients and servers looking to implement the ACP protocol.

#![warn(missing_docs)]

use std::error::Error;
use std::fmt::{Display, Formatter};

pub mod proto;

/// ACP errors which are relevant in both the client and server.
#[derive(Debug)]
pub enum AcpError {
    /// When a packet does not conform to the structure it was expected to.
    InvalidPacket,
}

impl Error for AcpError {}

impl Display for AcpError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::InvalidPacket => write!(f, "received packet with invalid structure"),
        }
    }
}
