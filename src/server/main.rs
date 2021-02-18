//! ACP server.
//!
//! Implements the `acp` protocol for fast file transfers.

#![warn(missing_docs)]

mod client;
pub mod router;

use crate::router::Router;
use anyhow::{Context, Result};
use thiserror::Error;
use tracing_subscriber;

/// Server's main function. Starts the router and manages top-level error handling.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_application_protos(b"\x07acp/0.1").unwrap();
    config
        .load_cert_chain_from_pem_file("cert.crt")
        .context("Failed to load TLS certificate")?;
    config
        .load_priv_key_from_pem_file("cert.key")
        .context("Failed to load TLS private key")?;
    config.set_max_recv_udp_payload_size(65535);
    config.set_max_send_udp_payload_size(65535);
    config.set_initial_max_data(1000000);
    config.set_initial_max_streams_bidi(1);
    config.set_initial_max_streams_uni(1);
    config.set_initial_max_stream_data_bidi_local(1000000);
    config.set_initial_max_stream_data_bidi_remote(1000000);
    config.set_initial_max_stream_data_uni(1000000);
    let router = Router::new("127.0.0.1:55280", config).await?;
    router.run().await
}

//TODO: Remove thiserror
#[derive(Debug, Error)]
pub enum AcpServerError {
    #[error("client input channel was dropped before the client could safely terminate")]
    ChannelDropped,
}
