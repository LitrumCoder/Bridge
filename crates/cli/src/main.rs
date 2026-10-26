#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::net::SocketAddr;
use std::process::ExitCode;

use bridge_ipc::{IpcClient, Request, Response, default_endpoint};
use bridge_protocol::DeviceId;
use clap::{Parser, Subcommand};
use serde_json::json;

// TODO(M3): pair; TODO(M6a): send; TODO(M8): pause, leave, rejoin.
#[derive(Parser)]
#[command(
    name = "bridge",
    version,
    about = "Command-line client of the Bridge daemon"
)]
struct Cli {
    /// IPC endpoint of the daemon (named pipe or socket path).
    #[arg(long, global = true)]
    ipc: Option<String>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// This device and its connections.
    Status,
    /// Trusted devices and whether they are connected.
    Devices,
    /// Keep a connection to a trusted device at a fixed address (manual add by IP).
    Connect { addr: SocketAddr, device_id: String },
    /// Stop the daemon.
    Shutdown,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bridge: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let request = match &cli.command {
        Command::Status => Request::Status,
        Command::Devices => Request::Devices,
        Command::Connect { addr, device_id } => Request::Connect {
            addr: *addr,
            device: DeviceId::from_base32(device_id).ok_or("invalid device id")?,
        },
        Command::Shutdown => Request::Shutdown,
    };
    let endpoint = cli.ipc.clone().unwrap_or_else(default_endpoint);
    let mut client = IpcClient::connect(&endpoint)
        .await
        .map_err(|e| format!("the daemon is not running ({endpoint}): {e}"))?;
    let response = client.request(request).await.map_err(|e| e.to_string())?;
    print_response(response, cli.json)
}

fn print_response(response: Response, as_json: bool) -> Result<(), String> {
    match response {
        Response::Error(e) => Err(e),
        Response::Ok => {
            if as_json {
                println!("{}", json!({ "ok": true }));
            } else {
                println!("ok");
            }
            Ok(())
        }
        Response::Status(s) => {
            if as_json {
                let peers: Vec<_> = s
                    .peers
                    .iter()
                    .map(|p| {
                        json!({
                            "id": p.id.to_base32(),
                            "name": p.name,
                            "platform": p.platform.0,
                            "version": p.app_version,
                            "rtt_us": p.rtt_us,
                            "clock_offset_us": p.clock_offset_us,
                        })
                    })
                    .collect();
                let doc = json!({
                    "id": s.device_id.to_base32(),
                    "name": s.name,
                    "listen": s.listen.to_string(),
                    "state": format!("{:?}", s.state),
                    "peers": peers,
                });
                println!("{doc}");
            } else {
                println!("{} ({})", s.name, s.device_id.fingerprint());
                println!("  id:     {}", s.device_id.to_base32());
                println!("  listen: {}", s.listen);
                println!("  state:  {:?}", s.state);
                if s.peers.is_empty() {
                    println!("  no connected devices");
                }
                for p in &s.peers {
                    let rtt = p
                        .rtt_us
                        .map_or("-".to_owned(), |r| format!("{:.2} ms", r as f64 / 1000.0));
                    println!("  ↔ {} ({}) rtt {rtt}", p.name, p.id.fingerprint());
                }
            }
            Ok(())
        }
        Response::Devices(devices) => {
            if as_json {
                let list: Vec<_> = devices
                    .iter()
                    .map(|d| json!({ "id": d.id.to_base32(), "name": d.name, "connected": d.connected }))
                    .collect();
                println!("{}", serde_json::Value::from(list));
            } else if devices.is_empty() {
                println!("no trusted devices");
            } else {
                for d in devices {
                    let state = if d.connected { "connected" } else { "offline" };
                    let name = d.name.unwrap_or_else(|| "?".into());
                    println!("{} {name:<24} {state}", d.id.fingerprint());
                }
            }
            Ok(())
        }
    }
}
