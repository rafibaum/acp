//! Common functionality for clients and servers looking to implement the ACP protocol.

#![warn(missing_docs)]

use std::error::Error;
use std::fmt::{Display, Formatter};

pub use crate::minmax::Minmax;

pub mod incoming;
mod minmax;
pub mod mpsc;
pub mod outgoing;
pub mod proto;

pub const INITIAL_WINDOW_SIZE: u64 = 4;

/// ACP errors which are relevant in both the client and server.
#[derive(Debug)]
pub enum AcpError {
    /// When a packet does not conform to the structure it was expected to.
    InvalidPacket,
    /// When a packet is received at a time it is not expected
    IllegalPacket,
    /// When a timeout has lapsed
    TimeoutLapsed,
}

impl Error for AcpError {}

impl Display for AcpError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::InvalidPacket => write!(f, "received packet with invalid structure"),
            AcpError::IllegalPacket => write!(f, "received a packet unexpectedly"),
            AcpError::TimeoutLapsed => write!(f, "timeout lapsed"),
        }
    }
}

/// Signal that a transfer has terminated. Used to indicate to the parent transfer that any
/// handles should be cleaned up.
#[derive(Debug)]
pub enum Terminated {
    /// Terminated incoming transfer
    Incoming(Vec<u8>),
    /// Terminated outgoing transfer
    Outgoing(Vec<u8>),
}
