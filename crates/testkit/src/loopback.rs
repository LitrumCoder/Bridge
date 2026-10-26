//! Two input-enabled nodes over real UDP on 127.0.0.1, for latency measurements.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge_core::input::CaptureEvent;
use bridge_daemon::{NodeBuilder, NodeHandle, TrustStore, TrustedPeer};
use bridge_feature_input::{InputConfig, InputFeature, Wake};
use bridge_platform::ScreenInfo;
use bridge_platform::mock::{Injected, MockPlatform};
use bridge_protocol::input::HidUsage;
use bridge_protocol::{DeviceId, FeatureId};
use bridge_transport::{DeviceIdentity, QuicOptions, QuicTransport};

pub struct InputPair {
    pub a: NodeHandle,
    pub b: NodeHandle,
    pub ma: MockPlatform,
    pub mb: MockPlatform,
    /// +1 when B is to the right of A.
    pub dir: i32,
}

fn node(identity: DeviceIdentity, peer: DeviceId, mock: &MockPlatform, name: &str) -> NodeHandle {
    let id = identity.id();
    let trust = TrustStore::new([TrustedPeer::own_device(peer)]);
    let transport = QuicTransport::bind(
        "127.0.0.1:0".parse().expect("addr"),
        identity,
        trust.clone(),
        QuicOptions::default(),
    )
    .expect("bind");
    let builder = NodeBuilder::new(name, trust);
    let local = builder.local_sender(FeatureId::INPUT);
    let feature = InputFeature::new(
        id,
        mock.platform(),
        InputConfig::default(),
        Arc::new(move || local.post(Wake)),
    )
    .expect("input");
    builder
        .screens(mock.screens.screens())
        .feature(feature)
        .expect("feature")
        .start(transport)
        .expect("start")
}

impl InputPair {
    /// Starts both nodes, connects them and moves the cursor onto B.
    pub async fn connected() -> Self {
        let (ia, ib) = (
            DeviceIdentity::generate().expect("id"),
            DeviceIdentity::generate().expect("id"),
        );
        let (id_a, id_b) = (ia.id(), ib.id());
        let (ma, mb) = (MockPlatform::new(), MockPlatform::new());
        let a = node(ia, id_b, &ma, "a");
        let b = node(ib, id_a, &mb, "b");
        a.dial(b.listen_addr(), id_b);
        assert!(
            a.wait_connected(id_b, true, Duration::from_secs(10)).await,
            "connect"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let dir = loop {
            if let Some(l) = a
                .layout()
                .filter(|l| l.screens.len() == 2 && Some(l) == b.layout().as_ref())
            {
                let x = |id| {
                    l.screens
                        .iter()
                        .find(|s| s.device == id)
                        .map(|s| s.rect_mm.x)
                        .unwrap_or(0.0)
                };
                break if x(id_b) > x(id_a) { 1 } else { -1 };
            }
            assert!(Instant::now() < deadline, "layout");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let x = if dir > 0 { 1918 } else { 1 };
        while !mb
            .injector
            .wait_for(Duration::from_millis(50), |l| {
                l.iter().any(|e| matches!(e, Injected::MoveAbs { .. }))
            })
            .await
        {
            assert!(Instant::now() < deadline, "cross");
            ma.capture.emit(CaptureEvent::Motion {
                x,
                y: 500,
                dx: 6 * dir,
                dy: 0,
            });
        }
        Self { a, b, ma, mb, dir }
    }

    /// One key press: time from the hook on A to the injection on B.
    pub async fn key_latency(&self, usage: HidUsage) -> Duration {
        self.mb.injector.clear();
        let start = Instant::now();
        self.ma.capture.emit(CaptureEvent::Key {
            usage,
            down: true,
            repeat: false,
        });
        let ok = self
            .mb
            .injector
            .wait_for(Duration::from_secs(5), |l| {
                l.contains(&Injected::Key { usage, down: true })
            })
            .await;
        let took = start.elapsed();
        assert!(ok, "key delivered");
        self.ma.capture.emit(CaptureEvent::Key {
            usage,
            down: false,
            repeat: false,
        });
        self.mb
            .injector
            .wait_for(Duration::from_secs(5), |l| {
                l.contains(&Injected::Key { usage, down: false })
            })
            .await;
        took
    }

    pub async fn shutdown(self) {
        self.a.shutdown().await;
        self.b.shutdown().await;
    }
}
