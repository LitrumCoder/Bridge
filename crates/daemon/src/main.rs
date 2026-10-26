#![allow(clippy::print_stderr)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bridge_daemon::{NodeBuilder, TrustStore, bind_transport, ipc_handler};
use bridge_discovery::MdnsDiscovery;
use bridge_ipc::{IpcClient, IpcServer, default_endpoint};
use bridge_protocol::DeviceId;
use bridge_transport::DeviceIdentity;
use clap::Parser;
use tokio::sync::Notify;
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "bridged", version, about = "Bridge background service")]
struct Args {
    /// Name shown to other devices (defaults to the host name).
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "0.0.0.0:47800")]
    listen: SocketAddr,
    /// Where the identity, trust store and settings live (default: per-user data directory).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Keep a connection to a paired device at ADDR: `ADDR=DEVICE_ID` (manual add by IP).
    #[arg(long = "connect", value_name = "ADDR=DEVICE_ID")]
    connect: Vec<String>,
    /// Do not announce or browse with mDNS (manual addresses only).
    #[arg(long)]
    no_mdns: bool,
    /// Announce only the fingerprint, not the device name.
    #[arg(long)]
    hide_name: bool,
    /// IPC endpoint (named pipe or socket path).
    #[arg(long)]
    ipc: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bridged: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let endpoint = args.ipc.clone().unwrap_or_else(default_endpoint);
    if IpcClient::connect(&endpoint).await.is_ok() {
        return Err("another bridged is already running for this user".into());
    }

    let data_dir = match args.data_dir {
        Some(d) => d,
        None => directories::ProjectDirs::from("", "", "Bridge")
            .ok_or("cannot determine the data directory")?
            .data_dir()
            .to_path_buf(),
    };
    let identity =
        DeviceIdentity::load_or_create(&data_dir.join("identity")).map_err(|e| e.to_string())?;
    let id = identity.id();
    let trust =
        TrustStore::open(&data_dir.join("trust.toml")).map_err(|e| format!("trust store: {e}"))?;

    let name = args.name.unwrap_or_else(host_name);
    let transport =
        bind_transport(args.listen, identity, trust.clone()).map_err(|e| e.to_string())?;
    let mut builder = NodeBuilder::new(name.clone(), trust).hide_name(args.hide_name);
    if !args.no_mdns {
        match MdnsDiscovery::start(id) {
            Ok(d) => builder = builder.discovery(d),
            Err(e) => warn!("mDNS is unavailable, only manual addresses will work: {e}"),
        }
    }
    let node = builder.start(transport).map_err(|e| e.to_string())?;
    info!(device = %id.to_base32(), %name, listen = %node.status().listen, "bridged started");

    for c in &args.connect {
        let (addr, peer) = c
            .split_once('=')
            .ok_or_else(|| format!("expected ADDR=DEVICE_ID, got {c}"))?;
        let addr: SocketAddr = addr
            .parse()
            .map_err(|e| format!("bad address {addr}: {e}"))?;
        let peer =
            DeviceId::from_base32(peer).ok_or_else(|| format!("invalid device id {peer}"))?;
        if node.trust().get(&peer).is_none() {
            warn!(%peer, "--connect ignored: the device is not paired");
            continue;
        }
        node.dial(addr, peer);
    }

    let shutdown = Arc::new(Notify::new());
    let server = IpcServer::bind(&endpoint).map_err(|e| format!("ipc endpoint {endpoint}: {e}"))?;
    tokio::spawn(server.serve(ipc_handler(node.clone(), shutdown.clone())));

    tokio::select! {
        _ = shutdown.notified() => info!("shutdown requested over ipc"),
        _ = tokio::signal::ctrl_c() => info!("interrupted"),
    }
    node.shutdown().await;
    Ok(())
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Bridge device".to_owned())
}
