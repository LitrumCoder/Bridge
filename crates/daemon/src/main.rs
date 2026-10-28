#![allow(clippy::print_stderr)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bridge_core::input::RouterEvent;
use bridge_daemon::{NodeBuilder, Services, TrustStore, bind_transport, ipc_handler};
use bridge_discovery::MdnsDiscovery;
use bridge_feature_clipboard::{ClipboardConfig, ClipboardFeature};
use bridge_feature_files::{CursorEvent, DragWiring, FilesConfig, FilesFeature};
use bridge_feature_input::{InputConfig, InputFeature, Wake};
use bridge_feature_midi::{MidiConfig, MidiFeature};
use bridge_ipc::UiEvent;
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
    /// Do not share the mouse and keyboard (no input hooks).
    #[arg(long)]
    no_input: bool,
    /// Do not share the clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Do not send or receive files.
    #[arg(long)]
    no_files: bool,
    /// Do not forward MIDI.
    #[arg(long)]
    no_midi: bool,
    /// Where received files go (default: Downloads/Bridge).
    #[arg(long)]
    download_dir: Option<PathBuf>,
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
    let profiles_path = data_dir.join("profiles.toml");
    let mut builder = NodeBuilder::new(name.clone(), trust)
        .hide_name(args.hide_name)
        .profiles(
            bridge_daemon::profiles::load(&profiles_path),
            Some(profiles_path),
        );
    let mut services = Services::default();
    let mut files = None;
    if !args.no_files {
        let post = builder
            .local_sender(bridge_protocol::FeatureId::FILES)
            .into_post();
        let (link, incoming) = builder.bulk();
        let downloads = args
            .download_dir
            .clone()
            .or_else(|| {
                directories::UserDirs::new()
                    .and_then(|u| u.download_dir().map(|d| d.join("Bridge")))
            })
            .unwrap_or_else(|| data_dir.join("received"));
        let mut config = FilesConfig::new(downloads);
        config.staging_dir = data_dir.join("staging");
        let (feature, handle) = FilesFeature::new(config, link, incoming, post);
        services.files = Some(handle);
        files = Some(feature);
    }
    let mut screens_source = None;
    if !args.no_input {
        match bridge_platform::native() {
            Ok(platform) => {
                let local = builder.local_sender(bridge_protocol::FeatureId::INPUT);
                let screens = platform.screens.clone();
                let mut feature = InputFeature::new(
                    id,
                    platform,
                    InputConfig::default(),
                    Arc::new(move || local.post(Wake)),
                )
                .map_err(|e| format!("input: {e}"))?;
                if let Some(f) = files.take() {
                    files = Some(with_drag(f, &feature));
                }
                let (files_events, ui) = (services.files.clone(), services.events.clone());
                feature = feature.on_router_event(Arc::new(move |e| {
                    let ev = match e {
                        RouterEvent::Entered { peer } => {
                            if let Some(h) = &files_events {
                                h.cursor(CursorEvent::Entered(*peer));
                            }
                            UiEvent::Entered { device: *peer }
                        }
                        RouterEvent::Returned { .. } => {
                            if let Some(h) = &files_events {
                                h.cursor(CursorEvent::Returned);
                            }
                            UiEvent::Returned
                        }
                        RouterEvent::Locked(on) => UiEvent::Locked(*on),
                        RouterEvent::Shake { device } => UiEvent::Shake { device: *device },
                    };
                    ui.push(ev);
                }));
                builder = builder
                    .screens(screens.screens())
                    .feature(feature)
                    .map_err(|e| e.to_string())?;
                screens_source = Some(screens);
            }
            Err(e) => warn!("mouse and keyboard sharing is unavailable: {e}"),
        }
    }
    if let Some(f) = files {
        builder = builder.feature(f).map_err(|e| e.to_string())?;
    }
    if !args.no_midi {
        match bridge_platform::native_midi() {
            Ok(ports) => {
                let post = builder
                    .local_sender(bridge_protocol::FeatureId::MIDI)
                    .into_post();
                let (feature, stats) = MidiFeature::new(ports, MidiConfig::new(name.clone()), post);
                services.midi = Some(stats);
                builder = builder.feature(feature).map_err(|e| e.to_string())?;
            }
            Err(e) => warn!("MIDI forwarding is unavailable: {e}"),
        }
    }
    if !args.no_clipboard {
        match bridge_platform::native_clipboard() {
            Ok(clipboard) => {
                let post = builder
                    .local_sender(bridge_protocol::FeatureId::CLIPBOARD)
                    .into_post();
                match ClipboardFeature::new(clipboard, ClipboardConfig::default(), post) {
                    Ok(f) => builder = builder.feature(f).map_err(|e| e.to_string())?,
                    Err(e) => warn!("clipboard sharing is unavailable: {e}"),
                }
            }
            Err(e) => warn!("clipboard sharing is unavailable: {e}"),
        }
    }
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

    if let Some(screens) = screens_source {
        // Monitors come and go (hot-plug, resolution); keep the shared layout current.
        let node = node.clone();
        tokio::spawn(async move {
            let mut last = screens.screens();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let now = screens.screens();
                if now != last {
                    info!(count = now.len(), "monitors changed");
                    node.set_screens(now.clone());
                    last = now;
                }
            }
        });
    }

    {
        // ADR-0016: the network fingerprint, for leaving and rejoining with profiles.
        let node = node.clone();
        tokio::spawn(async move {
            loop {
                let fp = tokio::task::spawn_blocking(|| {
                    bridge_platform::network::current_network().map(|n| {
                        let d = ring::digest::digest(&ring::digest::SHA256, &n.identity_bytes());
                        let mut fp = [0u8; 32];
                        fp.copy_from_slice(d.as_ref());
                        fp
                    })
                })
                .await
                .ok()
                .flatten();
                node.network_seen(fp);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

    let shutdown = Arc::new(Notify::new());
    let server = IpcServer::bind(&endpoint).map_err(|e| format!("ipc endpoint {endpoint}: {e}"))?;
    tokio::spawn(server.serve(ipc_handler(node.clone(), shutdown.clone(), services)));

    tokio::select! {
        _ = shutdown.notified() => info!("shutdown requested over ipc"),
        _ = tokio::signal::ctrl_c() => info!("interrupted"),
    }
    node.shutdown().await;
    Ok(())
}

/// Dragging files across edges needs both the OS drag services and the input router.
fn with_drag(files: FilesFeature, input: &InputFeature) -> FilesFeature {
    let control = input.control();
    let set_drag_active = Arc::new(move |on| control.set_drag_active(on));
    match bridge_platform::native_drag() {
        Ok((source, target)) => files.with_drag(DragWiring {
            source,
            target: Some(target),
            set_drag_active,
        }),
        Err(e) => {
            warn!("dragging files across devices is unavailable: {e}; dropped files are saved");
            files
        }
    }
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
