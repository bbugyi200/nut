#![forbid(unsafe_code)]

fn main() {
    nut_cli::command().get_matches();
    println!("Bootstrap only: monitoring and management commands are not implemented yet.");
}
