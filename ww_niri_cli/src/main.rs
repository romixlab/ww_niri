//! Thin: the command line and the calls live in the library crate; this just runs it and prints
//! an error.

use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime can be made");
    match runtime.block_on(ww_niri_cli::run(std::env::args_os())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let red = anstyle::AnsiColor::Red.on_default();
            use std::io::Write as _;
            let _ = writeln!(anstream::stderr(), "{red}wwn: {e:#}{red:#}");
            ExitCode::FAILURE
        }
    }
}
