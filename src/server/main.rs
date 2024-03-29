//! ACP server.
//!
//! Implements the `acp` protocol for fast file transfers.

#![warn(missing_docs)]

mod cli;
mod client;
mod router;

use crate::router::Router;
use anyhow::{Context, Result};
use thiserror::Error;

/// Server's main function. Starts the router and manages top-level error handling.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::build_cli().get_matches();

    let bind_addr = cli.value_of("bind").unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_application_protos(b"\x07acp/0.1").unwrap();
    config
        .load_cert_chain_from_pem_file("cert.crt")
        .context("Failed to load TLS certificate")?;
    config
        .load_priv_key_from_pem_file("cert.key")
        .context("Failed to load TLS private key")?;
    config.set_max_recv_udp_payload_size(1200);
    config.set_max_send_udp_payload_size(1200);
    config.set_initial_max_data(1000000);
    config.set_initial_max_streams_bidi(1);
    config.set_initial_max_streams_uni(1);
    config.set_initial_max_stream_data_bidi_local(1000000);
    config.set_initial_max_stream_data_bidi_remote(1000000);
    config.set_initial_max_stream_data_uni(1000000);
    config.set_max_idle_timeout(30 * 1000);
    config.enable_dgram(true, 512, 512);
    config
        .set_cc_algorithm_name(cli.value_of("congestion control").unwrap())
        .unwrap();

    let router = Router::new(bind_addr, config).await?;
    router.run().await
}

/// Error enum containing the errors generated in the ACP server.
//TODO: Remove thiserror
#[derive(Debug, Error)]
pub enum AcpServerError {
    /// When an async channel is illegally dropped without properly terminating
    #[error("client input channel was dropped before the client could safely terminate")]
    ChannelDropped,
}
