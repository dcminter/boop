mod cli;
mod client;
mod modes;
mod protocol;
mod server;
mod sessions;
mod sys;

use std::process::ExitCode;

fn main() -> ExitCode {
    let env_detach_key = std::env::var("BOOP_DETACH_KEY").ok();
    let outcome = cli::parse(
        std::env::args_os().skip(1).collect(),
        env_detach_key.as_deref(),
    )
    .map_err(|error| format!("{error}\n{}", cli::USAGE))
    .and_then(|options| client::run(options.action, options.detach_key));
    match outcome {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("boop: {error}");
            ExitCode::from(1)
        }
    }
}
