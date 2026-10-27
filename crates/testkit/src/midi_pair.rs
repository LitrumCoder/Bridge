//! Two MIDI-enabled nodes over real UDP on 127.0.0.1: an instrument on A, a DAW on B.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge_daemon::{NodeBuilder, NodeHandle, TrustStore, TrustedPeer};
use bridge_feature_midi::{MidiConfig, MidiFeature, MidiHandle};
use bridge_platform::mock::MockMidi;
use bridge_protocol::{DeviceId, FeatureId};
use bridge_transport::{DeviceIdentity, QuicOptions, QuicTransport};

pub const INSTRUMENT: &str = "Keys";

pub struct MidiPair {
    pub a: NodeHandle,
    pub b: NodeHandle,
    pub ma: Arc<MockMidi>,
    pub mb: Arc<MockMidi>,
    pub hb: MidiHandle,
    /// The port B's applications see.
    pub port: String,
}

fn node(
    identity: DeviceIdentity,
    peer: DeviceId,
    midi: Arc<MockMidi>,
    name: &str,
) -> (NodeHandle, MidiHandle) {
    let trust = TrustStore::new([TrustedPeer::own_device(peer)]);
    let transport = QuicTransport::bind(
        "127.0.0.1:0".parse().expect("addr"),
        identity,
        trust.clone(),
        QuicOptions::default(),
    )
    .expect("bind");
    let builder = NodeBuilder::new(name, trust);
    let post = builder.local_sender(FeatureId::MIDI).into_post();
    let (feature, handle) = MidiFeature::new(midi, MidiConfig::new(name), post);
    let node = builder
        .feature(feature)
        .expect("feature")
        .start(transport)
        .expect("start");
    (node, handle)
}

impl MidiPair {
    pub async fn connected() -> Self {
        crate::fine_timers();
        let (ia, ib) = (
            DeviceIdentity::generate().expect("id"),
            DeviceIdentity::generate().expect("id"),
        );
        let (id_a, id_b) = (ia.id(), ib.id());
        let ma = Arc::new(MockMidi::with_instrument(INSTRUMENT));
        let mb = Arc::new(MockMidi::default());
        let (a, _) = node(ia, id_b, ma.clone(), "studio");
        let (b, hb) = node(ib, id_a, mb.clone(), "laptop");
        a.dial(b.listen_addr(), id_b);
        assert!(
            a.wait_connected(id_b, true, Duration::from_secs(10)).await,
            "connected"
        );
        let port = format!("{INSTRUMENT} via studio");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !mb.virtual_ports().contains(&port) {
            assert!(Instant::now() < deadline, "B creates the port");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            a,
            b,
            ma,
            mb,
            hb,
            port,
        }
    }

    /// Plays one note on A's instrument and waits until B's application gets it.
    pub async fn note_latency(&self, note: u8) -> Duration {
        let before = self.mb.written(&self.port).len();
        let t = Instant::now();
        assert!(self.ma.play(INSTRUMENT, &[0x90, note & 0x7F, 100]));
        assert!(
            self.mb
                .wait_written(&self.port, before + 1, Duration::from_secs(5))
                .await,
            "note arrives"
        );
        let at = self.mb.written(&self.port)[before].0;
        let _ = self.ma.play(INSTRUMENT, &[0x80, note & 0x7F, 0]);
        self.mb
            .wait_written(&self.port, before + 2, Duration::from_secs(5))
            .await;
        at.saturating_duration_since(t)
    }

    pub async fn shutdown(self) {
        self.a.shutdown().await;
        self.b.shutdown().await;
    }
}
