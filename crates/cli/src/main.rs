use clap::Parser;

// TODO(M2): status, devices; TODO(M3): pair; TODO(M6a): send; TODO(M8): pause, leave, rejoin.
#[derive(Parser)]
#[command(
    name = "bridge",
    version,
    about = "Command-line client of the Bridge daemon"
)]
struct Cli {}

fn main() {
    Cli::parse();
}
