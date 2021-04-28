use clap::{App, Arg};

pub fn build_cli() -> App<'static, 'static> {
    App::new("acpd")
        .version("0.1")
        .author("Rafi Baum <rafi@ukbaums.com>")
        .arg(
            Arg::with_name("bind")
                .short("b")
                .long("bind")
                .help("Set the daemon bind address and port")
                .takes_value(true),
        )
}
