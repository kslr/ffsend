use clap::{App, Arg, SubCommand};

use cmd::arg::{ArgPassword, ArgUrl, CmdArg};

/// The download command definition.
pub struct CmdDownload;

impl CmdDownload {
    pub fn build<'a, 'b>() -> App<'a, 'b> {
        SubCommand::with_name("download")
            .about("Download files.")
            .visible_alias("d")
            .visible_alias("down")
            .arg(ArgUrl::build())
            .arg(ArgPassword::build())
            .arg(Arg::with_name("output")
                 .long("output")
                 .short("o")
                 .alias("output-file")
                 .alias("out")
                 .alias("file")
                 .value_name("PATH")
                 .help("The output file or directory"))
    }
}
