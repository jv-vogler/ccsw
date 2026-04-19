use std::process::ExitCode;

fn main() -> ExitCode {
    match ccsw::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ccsw: {err:#}");
            ExitCode::FAILURE
        }
    }
}
