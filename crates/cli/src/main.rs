#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::net::SocketAddr;
use std::process::ExitCode;

use bridge_ipc::{IpcClient, PairTarget, Request, Response, default_endpoint};
use bridge_protocol::{DeviceId, PermissionId};
use clap::{Parser, Subcommand};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use serde_json::json;

// TODO(M8): pause, leave, rejoin.
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
    /// Paired devices and whether they are connected.
    Devices,
    /// Devices announced on the local network.
    Nearby,
    /// Pair with another device, or show a code with --show.
    Pair {
        /// Show a code and a QR code on this device for two minutes.
        #[arg(long, conflicts_with_all = ["target", "uri"])]
        show: bool,
        /// Pair using a scanned or pasted `bridge://pair?...` link.
        #[arg(long, conflicts_with = "target")]
        uri: Option<String>,
        /// Fingerprint prefix of a nearby device, or IP:PORT.
        target: Option<String>,
        /// The 6-digit code shown on the other device.
        code: Option<String>,
    },
    /// Forget a paired device.
    Unpair { device: String },
    /// Show or change what a paired device may do to this one.
    Permissions {
        device: String,
        #[arg(long, value_parser = on_off)]
        input: Option<bool>,
        #[arg(long, value_parser = on_off)]
        files: Option<bool>,
        #[arg(long, value_parser = on_off)]
        clipboard: Option<bool>,
        #[arg(long, value_parser = on_off)]
        midi: Option<bool>,
    },
    /// Keep a connection to a paired device at a fixed address (manual add by IP).
    Connect { addr: SocketAddr, device_id: String },
    /// Send files or folders to a paired device (name or fingerprint prefix).
    Send {
        device: String,
        #[arg(required = true)]
        paths: Vec<std::path::PathBuf>,
        /// Wait and show progress until the transfer ends.
        #[arg(long)]
        wait: bool,
    },
    /// File transfers in progress and recently finished.
    Transfers,
    /// Cancel a transfer (id prefix from `bridge transfers`).
    Cancel { id: String },
    /// Stop the daemon.
    Shutdown,
}

fn on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => Err("expected on or off".into()),
    }
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
    let endpoint = cli.ipc.clone().unwrap_or_else(default_endpoint);
    let mut client = IpcClient::connect(&endpoint)
        .await
        .map_err(|e| format!("the daemon is not running ({endpoint}): {e}"))?;
    let mut call = async |req: Request| -> Result<Response, String> {
        match client.request(req).await.map_err(|e| e.to_string())? {
            Response::Error(e) => Err(e),
            other => Ok(other),
        }
    };

    let request = match cli.command {
        Command::Status => Request::Status,
        Command::Devices => Request::Devices,
        Command::Nearby => Request::Nearby,
        Command::Pair { show: true, .. } => Request::OpenPairing,
        Command::Pair { uri: Some(uri), .. } => Request::Pair {
            target: PairTarget::Uri(uri),
            code: String::new(),
        },
        Command::Pair {
            target: Some(target),
            code,
            ..
        } => {
            let code = code.ok_or("enter the 6-digit code shown on the other device")?;
            let target = match target.parse::<SocketAddr>() {
                Ok(addr) => PairTarget::Address(addr),
                Err(_) => PairTarget::Nearby(target),
            };
            Request::Pair { target, code }
        }
        Command::Pair { .. } => Request::Nearby,
        Command::Unpair { device } => Request::Unpair { device },
        Command::Permissions {
            device,
            input,
            files,
            clipboard,
            midi,
        } => {
            let changes = [
                (PermissionId::INPUT, input),
                (PermissionId::FILES, files),
                (PermissionId::CLIPBOARD, clipboard),
                (PermissionId::MIDI, midi),
            ];
            if changes.iter().all(|(_, c)| c.is_none()) {
                let Response::Devices(list) = call(Request::Devices).await? else {
                    return Err("unexpected reply".into());
                };
                let d = list
                    .iter()
                    .find(|d| d.id.to_base32().starts_with(&device.to_lowercase()))
                    .ok_or("no paired device matches")?;
                return print_permissions(d.id, &d.granted, cli.json);
            }
            let Response::Devices(list) = call(Request::Devices).await? else {
                return Err("unexpected reply".into());
            };
            let d = list
                .iter()
                .find(|d| d.id.to_base32().starts_with(&device.to_lowercase()))
                .ok_or("no paired device matches")?;
            let mut granted: Vec<u16> = d.granted.clone();
            for (perm, change) in changes {
                match change {
                    Some(true) if !granted.contains(&perm.0) => granted.push(perm.0),
                    Some(false) => granted.retain(|g| *g != perm.0),
                    _ => {}
                }
            }
            Request::SetPermissions { device, granted }
        }
        Command::Connect { addr, device_id } => Request::Connect {
            addr,
            device: DeviceId::from_base32(&device_id).ok_or("invalid device id")?,
        },
        Command::Shutdown => Request::Shutdown,
        Command::Send {
            device,
            paths,
            wait,
        } => {
            let paths = paths
                .iter()
                .map(|p| {
                    std::fs::canonicalize(p)
                        .map(|p| p.to_string_lossy().into_owned())
                        .map_err(|e| format!("{}: {e}", p.display()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let Response::Transfer { id } = call(Request::Send { device, paths }).await? else {
                return Err("unexpected reply".into());
            };
            if !wait {
                return print_response(Response::Transfer { id }, cli.json);
            }
            loop {
                let Response::Transfers(list) = call(Request::Transfers).await? else {
                    return Err("unexpected reply".into());
                };
                let Some(t) = list.into_iter().find(|t| t.id == id) else {
                    return Err("the transfer disappeared".into());
                };
                if !cli.json {
                    eprint!(
                        "\r{:>6.1}%  {}  {}   ",
                        percent(&t),
                        t.state,
                        t.names.join(", ")
                    );
                }
                if t.state != "running" {
                    if !cli.json {
                        eprintln!();
                    }
                    let ok = t.state == "done";
                    print_response(Response::Transfers(vec![t]), cli.json)?;
                    return if ok {
                        Ok(())
                    } else {
                        Err("the transfer did not complete".into())
                    };
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        Command::Transfers => Request::Transfers,
        Command::Cancel { id } => Request::CancelTransfer { id },
    };
    let response = call(request).await?;
    print_response(response, cli.json)
}

fn percent(t: &bridge_ipc::TransferEntry) -> f64 {
    if t.bytes == 0 {
        100.0
    } else {
        t.done_bytes as f64 * 100.0 / t.bytes as f64
    }
}

fn permission_names(granted: &[u16]) -> Vec<&'static str> {
    [(1, "input"), (2, "files"), (3, "clipboard"), (4, "midi")]
        .into_iter()
        .filter(|(id, _)| granted.contains(id))
        .map(|(_, n)| n)
        .collect()
}

fn print_permissions(id: DeviceId, granted: &[u16], as_json: bool) -> Result<(), String> {
    let names = permission_names(granted);
    if as_json {
        println!("{}", json!({ "id": id.to_base32(), "granted": names }));
    } else {
        println!(
            "{}: {}",
            id.fingerprint(),
            if names.is_empty() {
                "nothing".into()
            } else {
                names.join(", ")
            }
        );
    }
    Ok(())
}

fn print_response(response: Response, as_json: bool) -> Result<(), String> {
    match response {
        Response::Error(e) => return Err(e),
        Response::Ok => {
            if as_json {
                println!("{}", json!({ "ok": true }));
            } else {
                println!("ok");
            }
        }
        Response::Transfer { id } => {
            if as_json {
                println!("{}", json!({ "transfer": id }));
            } else {
                println!(
                    "transfer {} started; `bridge transfers` shows progress",
                    &id[..8.min(id.len())]
                );
            }
        }
        Response::Transfers(list) => {
            if as_json {
                let v: Vec<_> = list
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id, "device": t.device.to_base32(), "outgoing": t.outgoing,
                            "items": t.items, "bytes": t.bytes, "done_bytes": t.done_bytes,
                            "state": t.state, "names": t.names,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::Array(v));
            } else if list.is_empty() {
                println!("no transfers");
            } else {
                for t in &list {
                    println!(
                        "{}  {} {}  {:>6.1}%  {} items, {} bytes  {}  {}",
                        &t.id[..8.min(t.id.len())],
                        if t.outgoing { "→" } else { "←" },
                        t.device.fingerprint(),
                        percent(t),
                        t.items,
                        t.bytes,
                        t.state,
                        t.names.join(", ")
                    );
                }
            }
        }
        Response::Paired { device } => {
            if as_json {
                println!("{}", json!({ "paired": device.to_base32() }));
            } else {
                println!(
                    "paired with {} — the devices will now connect automatically",
                    device.fingerprint()
                );
            }
        }
        Response::PairingInvite {
            code,
            qr_uri,
            expires_secs,
        } => {
            if as_json {
                println!(
                    "{}",
                    json!({ "code": code, "uri": qr_uri, "expires_secs": expires_secs })
                );
            } else {
                let qr = QrCode::new(qr_uri.as_bytes())
                    .map(|c| {
                        c.render::<Dense1x2>()
                            .dark_color(Dense1x2::Light)
                            .light_color(Dense1x2::Dark)
                            .quiet_zone(true)
                            .build()
                    })
                    .unwrap_or_default();
                println!("{qr}");
                println!(
                    "Pairing code: {} {}",
                    &code[..3.min(code.len())],
                    &code[3.min(code.len())..]
                );
                println!("Link (for the phone or `bridge pair --uri`): {qr_uri}");
                println!("Valid for {expires_secs} s, for one device.");
            }
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
        }
        Response::Devices(devices) => {
            if as_json {
                let list: Vec<_> = devices
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d.id.to_base32(),
                            "name": d.name,
                            "connected": d.connected,
                            "granted": permission_names(&d.granted),
                            "last_seen": d.last_seen,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::from(list));
            } else if devices.is_empty() {
                println!(
                    "no paired devices — run `bridge pair --show` on one and `bridge pair` on the other"
                );
            } else {
                for d in devices {
                    let state = if d.connected { "connected" } else { "offline" };
                    let name = d.name.unwrap_or_else(|| "?".into());
                    let perms = permission_names(&d.granted).join(",");
                    println!("{} {name:<24} {state:<10} {perms}", d.id.fingerprint());
                }
            }
        }
        Response::Nearby(list) => {
            if as_json {
                let list: Vec<_> = list
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d.id.to_base32(),
                            "name": d.name,
                            "addrs": d.addrs.iter().map(ToString::to_string).collect::<Vec<_>>(),
                            "paired": d.paired,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::from(list));
            } else if list.is_empty() {
                println!("no devices found on this network (try `bridge pair <IP:PORT> <CODE>`)");
            } else {
                for d in list {
                    let name = d.name.unwrap_or_else(|| "(hidden name)".into());
                    let tag = if d.paired { "paired" } else { "not paired" };
                    let addr = d.addrs.first().map(ToString::to_string).unwrap_or_default();
                    println!("{} {name:<24} {tag:<11} {addr}", d.id.fingerprint());
                }
            }
        }
    }
    Ok(())
}
