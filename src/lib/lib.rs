//! Common functionality for clients and servers looking to implement the ACP protocol.

#![warn(missing_docs)]

use std::error::Error;
use std::fmt::{Display, Formatter};

pub mod proto;

#[derive(Debug)]
pub enum AcpError {
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
