use clap::{App, AppSettings, Arg, SubCommand};

pub fn build_cli() -> App<'static, 'static> {
    App::new("acp")
        .version("0.1")
        .author("Rafi Baum <rafi@ukbaums.com>")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg(
            Arg::with_name("SERVER")
                .help("Server to connect to")
                .required(true)
                .index(1),
        )
        .subcommand(
            SubCommand::with_name("get")
                .about("download a file")
                .arg(
                    Arg::with_name("integrity")
                        .short("c")
                        .long("check")
                        .help("Whether to perform integrity checking")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("SOURCE")
                        .help("File to download")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::with_name("DESTINATION")
                        .help("File output path")
                        .required(true)
                        .index(2),
                ),
        )
        .subcommand(
            SubCommand::with_name("put")
                .about("upload a file")
                .arg(
                    Arg::with_name("integrity")
                        .short("c")
                        .long("check")
                        .help("Whether to perform integrity checking")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("SOURCE")
                        .help("File to upload")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::with_name("DESTINATION")
                        .help("File upload destination")
                        .required(true)
                        .index(2),
                ),
        )
}
