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
                .takes_value(true)
                .default_value("127.0.0.1:55280"),
        )
        .arg(
            Arg::with_name("congestion control")
                .short("C")
                .long("congestion-control")
                .help("Which congestion control algorithm to use")
                .takes_value(true)
                .possible_values(&["reno", "cubic", "bbr"])
                .default_value("bbr"),
        )
}
