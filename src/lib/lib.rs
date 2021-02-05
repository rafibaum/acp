//! Common functionality for clients and servers looking to implement the ACP protocol.

#![warn(missing_docs)]

use prost::alloc::fmt::Formatter;
use std::error::Error;
use std::fmt::Display;

pub mod proto;

/// The return type used for ACP error handling.
pub type Result<T> = std::result::Result<T, AcpError>;

/// Errors which can occur during ACP execution.
#[derive(Debug)]
pub enum AcpError {
    /// Errors raised from internal I/O constructs.
    IoError(std::io::Error),
    /// Errors due to a poisoned mutex.
    PoisonedMutexError,
    /// Errors during packet decoding
    PacketDecodeError(prost::DecodeError),
}

impl Display for AcpError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::IoError(err) => {
                write!(f, "IO Error: {}", err)
            }
            AcpError::PoisonedMutexError => {
                write!(f, "Internal concurrency error, poisoned mutex!")
            }
            AcpError::PacketDecodeError(err) => {
                write!(f, "Packet decoding error: {}", err)
            }
        }
    }
}

impl Error for AcpError {}

impl From<std::io::Error> for AcpError {
    fn from(err: std::io::Error) -> Self {
        AcpError::IoError(err)
    }
}

impl<T> From<std::sync::PoisonError<T>> for AcpError {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        AcpError::PoisonedMutexError
    }
}

impl From<prost::DecodeError> for AcpError {
    fn from(err: prost::DecodeError) -> Self {
        AcpError::PacketDecodeError(err)
    }
}
