use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut listen = "127.0.0.1:43493".to_owned();
    let mut fixture = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => match arguments.next() {
                Some(value) => listen = value,
                None => return usage_error("--listen needs an address"),
            },
            "--fixture" => match arguments.next() {
                Some(value) => fixture = Some(PathBuf::from(value)),
                None => return usage_error("--fixture needs a path"),
            },
            "-h" | "--help" => {
                println!("Usage: nut-fake-ups-d [--listen 127.0.0.1:43493] --fixture PATH");
                println!("Serve read-only scripted UPS data on loopback for demos and tests.");
                return ExitCode::SUCCESS;
            }
            _ => return usage_error(&format!("unknown argument {argument:?}")),
        }
    }
    let Some(fixture) = fixture else {
        return usage_error("--fixture is required");
    };
    match nut_cli::fake_upsd::run_fake_upsd(&listen, &fixture) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nut-fake-ups-d: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("nut-fake-ups-d: {message}");
    eprintln!("Usage: nut-fake-ups-d [--listen 127.0.0.1:43493] --fixture PATH");
    ExitCode::from(2)
}
