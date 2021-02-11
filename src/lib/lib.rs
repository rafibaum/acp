//! Common functionality for clients and servers looking to implement the ACP protocol.

#![warn(missing_docs)]

use std::sync::PoisonError;
use thiserror::Error;

pub mod proto;

/// The return type used for ACP error handling.
pub type Result<T> = std::result::Result<T, AcpError>;

/// Errors which can occur during ACP execution.
#[derive(Debug, Error)]
pub enum AcpError {
    /// Used where a mutex was poisoned. The standard library error contains the MutexGuard itself
    /// which does not implement the Send trait, so this is used to drop that guard entirely.
    #[error("a mutex was poisoned")]
    PoisonedMutex,
}

impl<T> From<PoisonError<T>> for AcpError {
    fn from(_: PoisonError<T>) -> AcpError {
        AcpError::PoisonedMutex
    }
}
