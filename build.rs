use std::io::Result;

fn main() -> Result<()> {
    let mut config = prost_build::Config::new();
    config.out_dir("src/lib/proto/");
    config.compile_protos(&["src/lib/proto/packets.proto"], &["src/"])?;
    Ok(())
}
