use clap::Shell;
use std::env;
use std::io::Result;

fn main() -> Result<()> {
    build_protobuf()?;
    build_completions();
    Ok(())
}

fn build_protobuf() -> Result<()> {
    let mut config = prost_build::Config::new();
    config.out_dir("src/lib/proto/");
    config.compile_protos(&["src/lib/proto/packets.proto"], &["src/"])
}

fn build_completions() {
    let outdir = env::var("OUT_DIR").unwrap();

    let mut client = cli::client::build_cli();
    client.gen_completions("acp", Shell::Bash, outdir.clone());

    let mut server = cli::server::build_cli();
    server.gen_completions("acpd", Shell::Bash, outdir);
}

mod cli {
    pub mod client {
        include!("src/client/cli.rs");
    }

    pub mod server {
        include!("src/server/cli.rs");
    }
}
