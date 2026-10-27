//! MIDI forwarding (ARCHITECTURE §8.4, ADR-0009).
//!
//! The device with an instrument shares its MIDI ports: what the instrument plays goes
//! to peers, stamped with the capture time on the sender's clock. A peer offers it to
//! its applications through a virtual source, played out by a jitter buffer on a
//! thread of its own (the node's tick is far too coarse for a 3 ms buffer). The other
//! way, a virtual destination carries what a DAW plays back to the instrument. Held
//! notes are released whenever a peer goes away.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bridge_core::midi::{JitterBuffer, JitterConfig, JitterStats, NoteTracker};
use bridge_core::{
    Feature, FeatureCx, FeatureError, FeatureInfo, LocalPost, Micros, PeerDownReason, PeerId,
};
use bridge_platform::{MidiIn, MidiOut, MidiPorts, MidiSink};
use bridge_protocol::midi::{
    Event, Events, MAX_BATCH, MidiMessage, PortDirection, PortInfo, Ports,
};
use bridge_protocol::{
    BoundedBytes, BoundedString, BoundedVec, ChannelClass, FeatureId, PermissionId,
};
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub struct MidiConfig {
    /// Shown in the names of the ports peers create ("<port> via <device>").
    pub device_name: String,
    /// Share every MIDI input and output of this device.
    pub share: bool,
    pub jitter: JitterConfig,
    /// How long a lost peer's ports stay open (silent) waiting for it to come back.
    pub linger: Duration,
}

impl MidiConfig {
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            device_name: device_name.into(),
            share: true,
            jitter: JitterConfig::default(),
            linger: Duration::from_secs(10),
        }
    }
}

/// One playback stream: a remote instrument into a virtual source, or a remote DAW
/// into a local instrument.
type Key = (PeerId, u16, bool);

enum Cmd {
    Open(Key, Box<dyn MidiOut>),
    Push {
        key: Key,
        arrived: u64,
        sent: u64,
        msg: Vec<u8>,
    },
    Silence(Key),
    Close(Key),
}

struct Stream {
    jitter: JitterBuffer,
    notes: NoteTracker,
    out: Box<dyn MidiOut>,
}

#[derive(Clone, Debug)]
pub struct StreamStats {
    pub peer: PeerId,
    pub port: u16,
    pub to_instrument: bool,
    pub stats: JitterStats,
}

type StatsMap = Arc<Mutex<HashMap<Key, JitterStats>>>;

fn micros_since(epoch: Instant) -> u64 {
    epoch.elapsed().as_micros() as u64
}

fn play_out(s: &mut Stream, msg: &[u8]) {
    s.notes.observe(msg);
    if let Err(e) = s.out.send(msg) {
        debug!("MIDI send failed: {e}");
    }
}

/// The playback thread: sleeps until the next event is due, then sends it.
fn player(rx: Receiver<Cmd>, epoch: Instant, config: JitterConfig, stats: StatsMap) {
    bridge_platform::fine_timers();
    let mut streams: HashMap<Key, Stream> = HashMap::new();
    loop {
        let now = micros_since(epoch);
        let next = streams.values().filter_map(|s| s.jitter.next_due()).min();
        let cmd = match next {
            Some(t) if t <= now => None,
            Some(t) if t - now < 1_500 => {
                // The last stretch is spun: sleeps overshoot by up to a millisecond.
                match rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        std::thread::yield_now();
                        None
                    }
                    Err(_) => return,
                }
            }
            Some(t) => match rx.recv_timeout(Duration::from_micros(t - now - 1_000)) {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => return,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        match cmd {
            Some(Cmd::Open(key, out)) => {
                streams.insert(
                    key,
                    Stream {
                        jitter: JitterBuffer::new(config),
                        notes: NoteTracker::default(),
                        out,
                    },
                );
            }
            Some(Cmd::Push {
                key,
                arrived,
                sent,
                msg,
            }) => {
                if let Some(s) = streams.get_mut(&key) {
                    s.jitter.push(arrived, sent, msg);
                    stats
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(key, s.jitter.stats());
                }
            }
            Some(Cmd::Silence(key)) => {
                if let Some(s) = streams.get_mut(&key) {
                    // Whatever is queued still plays (it was played on the instrument), then
                    // everything held is released.
                    for m in s.jitter.drain() {
                        play_out(s, &m);
                    }
                    for m in s.notes.release_all() {
                        let _ = s.out.send(&m);
                    }
                }
            }
            Some(Cmd::Close(key)) => {
                if let Some(mut s) = streams.remove(&key) {
                    for m in s.notes.release_all() {
                        let _ = s.out.send(&m);
                    }
                }
                stats.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
            }
            None => {}
        }
        let now = micros_since(epoch);
        for s in streams.values_mut() {
            while let Some(m) = s.jitter.pop_due(now) {
                play_out(s, &m);
            }
        }
    }
}

enum Local {
    Captured {
        port: u16,
        at: Instant,
        msg: Vec<u8>,
    },
    FromDaw {
        peer: PeerId,
        port: u16,
        at: Instant,
        msg: Vec<u8>,
    },
}

struct Capture {
    post: LocalPost,
    port: u16,
}

impl MidiSink for Capture {
    fn on_midi(&self, at: Instant, msg: &[u8]) {
        (self.post)(Box::new(Local::Captured {
            port: self.port,
            at,
            msg: msg.to_vec(),
        }));
    }
}

struct FromDaw {
    post: LocalPost,
    peer: PeerId,
    port: u16,
}

impl MidiSink for FromDaw {
    fn on_midi(&self, at: Instant, msg: &[u8]) {
        (self.post)(Box::new(Local::FromDaw {
            peer: self.peer,
            port: self.port,
            at,
            msg: msg.to_vec(),
        }));
    }
}

/// A port this device shares.
struct Own {
    info: PortInfo,
    _input: Option<Box<dyn MidiIn>>,
}

/// A port a peer shares, as it exists here.
struct Remote {
    info: PortInfo,
    _dest: Option<Box<dyn MidiIn>>,
}

struct PeerPorts {
    ports: Vec<Remote>,
    lost_at: Option<Micros>,
}

/// Statistics for the UI and tests.
#[derive(Clone)]
pub struct MidiHandle {
    stats: StatsMap,
}

impl MidiHandle {
    pub fn streams(&self) -> Vec<StreamStats> {
        self.stats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|((peer, port, to), stats)| StreamStats {
                peer: *peer,
                port: *port,
                to_instrument: *to,
                stats: *stats,
            })
            .collect()
    }
}

pub struct MidiFeature {
    ports: Arc<dyn MidiPorts>,
    post: LocalPost,
    config: MidiConfig,
    own: Vec<Own>,
    /// Local instrument outputs by port id.
    outputs: HashMap<u16, String>,
    /// Playback streams already opened into local instruments, per peer.
    opened: HashSet<Key>,
    remote: HashMap<PeerId, PeerPorts>,
    player: Sender<Cmd>,
    epoch: Instant,
}

impl MidiFeature {
    pub fn new(
        ports: Arc<dyn MidiPorts>,
        config: MidiConfig,
        post: LocalPost,
    ) -> (Self, MidiHandle) {
        let epoch = Instant::now();
        let (tx, rx) = channel();
        let stats = StatsMap::default();
        let (s, jitter) = (stats.clone(), config.jitter);
        let _ = std::thread::Builder::new()
            .name("bridge-midi-player".into())
            .spawn(move || player(rx, epoch, jitter, s));
        let mut f = Self {
            ports,
            post,
            config,
            own: Vec::new(),
            outputs: HashMap::new(),
            opened: HashSet::new(),
            remote: HashMap::new(),
            player: tx,
            epoch,
        };
        if f.config.share {
            f.share_local();
        }
        (f, MidiHandle { stats })
    }

    /// Opens every local input and output for sharing; ids are stable per name order.
    fn share_local(&mut self) {
        let mut id = 0u16;
        for name in self.ports.inputs() {
            id += 1;
            let sink = Arc::new(Capture {
                post: self.post.clone(),
                port: id,
            });
            match self.ports.open_input(&name, sink) {
                Ok(i) => self.own.push(Own {
                    info: port_info(id, &name, PortDirection::FromInstrument),
                    _input: Some(i),
                }),
                Err(e) => warn!(%name, "cannot open MIDI input: {e}"),
            }
        }
        for name in self.ports.outputs() {
            id += 1;
            self.own.push(Own {
                info: port_info(id, &name, PortDirection::ToInstrument),
                _input: None,
            });
            self.outputs.insert(id, name);
        }
    }

    fn ports_message(&self) -> Option<MidiMessage> {
        let ports =
            BoundedVec::try_from_vec(self.own.iter().map(|o| o.info.clone()).collect()).ok()?;
        Some(
            Ports {
                device: BoundedString::truncating(&self.config.device_name),
                ports,
            }
            .into(),
        )
    }

    fn send(cx: &mut FeatureCx<'_>, peer: PeerId, msg: MidiMessage) {
        match msg.to_envelope() {
            Ok(env) => cx.send_encoded(peer, env.kind, env.payload),
            Err(e) => warn!("cannot encode a MIDI message: {e}"),
        }
    }

    /// Converts a capture instant into this node's clock.
    fn node_time(cx: &FeatureCx<'_>, at: Instant) -> u64 {
        cx.now().0.saturating_sub(at.elapsed().as_micros() as u64)
    }

    fn event(ts: u64, msg: Vec<u8>) -> Option<BoundedVec<Event, MAX_BATCH>> {
        let data = BoundedBytes::new(msg)?;
        BoundedVec::try_from_vec(vec![Event { ts, data }]).ok()
    }

    fn on_ports(&mut self, cx: &FeatureCx<'_>, peer: PeerId, p: Ports) {
        let old = self
            .remote
            .remove(&peer)
            .map(|r| r.ports)
            .unwrap_or_default();
        let mut kept = Vec::new();
        for info in p.ports.into_vec() {
            if let Some(r) = old.iter().position(|r| r.info == info) {
                kept.push(r);
                continue;
            }
            let name = format!("{} via {}", info.name, p.device);
            let to = info.direction == PortDirection::ToInstrument;
            let key = (peer, info.id, to);
            let dest = if to {
                let sink = Arc::new(FromDaw {
                    post: self.post.clone(),
                    peer,
                    port: info.id,
                });
                match self.ports.virtual_destination(&name, sink) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        warn!(%name, "cannot create a MIDI port: {e}");
                        continue;
                    }
                }
            } else {
                match self.ports.virtual_source(&name) {
                    Ok(out) => {
                        let _ = self.player.send(Cmd::Open(key, out));
                        None
                    }
                    Err(e) => {
                        warn!(%name, "cannot create a MIDI port: {e}");
                        continue;
                    }
                }
            };
            self.remote
                .entry(peer)
                .or_insert_with(|| PeerPorts {
                    ports: Vec::new(),
                    lost_at: None,
                })
                .ports
                .push(Remote { info, _dest: dest });
        }
        let mut old: Vec<Option<Remote>> = old.into_iter().map(Some).collect();
        let entry = self.remote.entry(peer).or_insert_with(|| PeerPorts {
            ports: Vec::new(),
            lost_at: None,
        });
        for i in kept {
            if let Some(r) = old[i].take() {
                entry.ports.push(r);
            }
        }
        for r in old.into_iter().flatten() {
            let _ = self.player.send(Cmd::Close((peer, r.info.id, false)));
        }
        entry.lost_at = None;
        debug!(%peer, ports = entry.ports.len(), at = cx.now().0, "MIDI ports of a peer");
    }

    fn on_events(&mut self, cx: &FeatureCx<'_>, peer: PeerId, e: Events) {
        let key = (peer, e.port, e.to_instrument);
        if e.to_instrument {
            // A DAW over there plays one of our instruments: one stream per peer, so
            // its notes can be released on its own.
            let Some(name) = self.outputs.get(&e.port).cloned() else {
                return;
            };
            if !self.opened.contains(&key) {
                match self.ports.open_output(&name) {
                    Ok(out) => {
                        let _ = self.player.send(Cmd::Open(key, out));
                        self.opened.insert(key);
                    }
                    Err(e) => {
                        warn!(%name, "cannot open MIDI output: {e}");
                        return;
                    }
                }
            }
        } else {
            let known = self.remote.get(&peer).is_some_and(|r| {
                r.ports.iter().any(|p| {
                    p.info.id == e.port && p.info.direction == PortDirection::FromInstrument
                })
            });
            if !known {
                return;
            }
        }
        let arrived = micros_since(self.epoch);
        let node_now = cx.now().0 as i64;
        let offset = cx.peer(&peer).and_then(|v| v.clock_offset);
        for ev in e.events.into_vec() {
            // Sender clock → ours → the player's; without clock sync yet, play on arrival.
            let sent = match offset {
                Some(off) => (arrived as i64 - (node_now - (ev.ts as i64 - off))).max(0) as u64,
                None => arrived,
            };
            let _ = self.player.send(Cmd::Push {
                key,
                arrived,
                sent,
                msg: ev.data.into_vec(),
            });
        }
    }
}

fn port_info(id: u16, name: &str, direction: PortDirection) -> PortInfo {
    PortInfo {
        id,
        name: BoundedString::truncating(name),
        direction,
    }
}

impl Feature for MidiFeature {
    fn info(&self) -> FeatureInfo {
        FeatureInfo {
            id: FeatureId::MIDI,
            name: "midi",
            versions: 1..=1,
            channel: ChannelClass::Midi,
            permission: Some(PermissionId::MIDI),
        }
    }

    fn on_peer_up(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, _version: u16) {
        if let Some(r) = self.remote.get_mut(&peer) {
            r.lost_at = None;
        }
        if cx.peer(&peer).is_some_and(|p| p.allows_us)
            && !self.own.is_empty()
            && let Some(m) = self.ports_message()
        {
            Self::send(cx, peer, m);
        }
    }

    fn on_peer_update(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId) {
        self.on_peer_up(cx, peer, 1);
    }

    fn on_peer_down(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, reason: PeerDownReason) {
        let keys: Vec<Key> = self
            .remote
            .get(&peer)
            .map(|r| r.ports.iter().map(|p| (peer, p.info.id, false)).collect())
            .unwrap_or_default();
        let into_instruments: Vec<Key> = self
            .opened
            .iter()
            .filter(|k| k.0 == peer)
            .copied()
            .collect();
        for k in keys.iter().chain(&into_instruments) {
            let _ = self.player.send(Cmd::Silence(*k));
        }
        if matches!(reason, PeerDownReason::Left | PeerDownReason::Revoked) {
            self.close_peer(peer);
        } else if let Some(r) = self.remote.get_mut(&peer) {
            r.lost_at = Some(cx.now());
        }
    }

    fn on_message(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        match MidiMessage::decode(kind, payload)? {
            MidiMessage::Ports(p) => self.on_ports(cx, peer, p),
            MidiMessage::Events(e) => self.on_events(cx, peer, e),
        }
        Ok(())
    }

    fn on_local(&mut self, cx: &mut FeatureCx<'_>, event: Box<dyn std::any::Any + Send>) {
        let Ok(event) = event.downcast::<Local>() else {
            return;
        };
        match *event {
            Local::Captured { port, at, msg } => {
                let ts = Self::node_time(cx, at);
                let Some(events) = Self::event(ts, msg) else {
                    return;
                };
                let peers: Vec<PeerId> = cx
                    .peers()
                    .iter()
                    .filter(|p| p.allows_us)
                    .map(|p| p.id)
                    .collect();
                for peer in peers {
                    Self::send(
                        cx,
                        peer,
                        Events {
                            port,
                            to_instrument: false,
                            events: events.clone(),
                        }
                        .into(),
                    );
                }
            }
            Local::FromDaw {
                peer,
                port,
                at,
                msg,
            } => {
                let ts = Self::node_time(cx, at);
                if let Some(events) = Self::event(ts, msg) {
                    Self::send(
                        cx,
                        peer,
                        Events {
                            port,
                            to_instrument: true,
                            events,
                        }
                        .into(),
                    );
                }
            }
        }
    }

    fn on_tick(&mut self, cx: &mut FeatureCx<'_>) {
        let linger = self.config.linger;
        let now = cx.now();
        let gone: Vec<PeerId> = self
            .remote
            .iter()
            .filter(|(_, r)| r.lost_at.is_some_and(|t| now.saturating_sub(t) > linger))
            .map(|(p, _)| *p)
            .collect();
        for p in gone {
            self.close_peer(p);
        }
    }
}

impl MidiFeature {
    fn close_peer(&mut self, peer: PeerId) {
        if let Some(r) = self.remote.remove(&peer) {
            for p in r.ports {
                let _ = self.player.send(Cmd::Close((peer, p.info.id, false)));
            }
        }
        let mine: Vec<Key> = self
            .opened
            .iter()
            .filter(|k| k.0 == peer)
            .copied()
            .collect();
        for k in mine {
            self.opened.remove(&k);
            let _ = self.player.send(Cmd::Close(k));
        }
    }
}
