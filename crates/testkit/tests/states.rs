//! Integration: Pause, Leave and Rejoin on three nodes (ARCHITECTURE §4.2).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use bridge_core::input::CaptureEvent;
use bridge_core::state::StateCommand;
use bridge_feature_clipboard::{ClipboardConfig, ClipboardFeature};
use bridge_feature_input::{InputConfig, InputFeature, Wake};
use bridge_platform::mock::{Injected, MockPlatform};
use bridge_platform::{ClipFormat, ScreenInfo};
use bridge_protocol::FeatureId;
use bridge_protocol::control::DeviceState;
use bridge_testkit::{Network, NodeSpec, TestNode, mutually_trusting};

const WAIT: Duration = Duration::from_secs(15);

fn start(spec: NodeSpec, name: &str, mock: &MockPlatform) -> TestNode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let id = spec.identity.id();
    let platform = mock.platform();
    let screens = mock.screens.screens();
    let clipboard = mock.clipboard.clone();
    spec.start(name, move |b| {
        let local = b.local_sender(FeatureId::INPUT);
        let input = InputFeature::new(
            id,
            platform,
            InputConfig::default(),
            Arc::new(move || local.post(Wake)),
        )
        .unwrap();
        let post = b.local_sender(FeatureId::CLIPBOARD).into_post();
        let clip = ClipboardFeature::new(clipboard, ClipboardConfig::default(), post).unwrap();
        b.screens(screens)
            .feature(input)
            .unwrap()
            .feature(clip)
            .unwrap()
    })
}

struct Three {
    a: TestNode,
    b: TestNode,
    c: TestNode,
    ma: MockPlatform,
    mb: MockPlatform,
    mc: MockPlatform,
}

async fn three() -> Three {
    let net = Network::new(701);
    let (ma, mb, mc) = (
        MockPlatform::new(),
        MockPlatform::new(),
        MockPlatform::new(),
    );
    let mut specs = mutually_trusting(&net, 3).unwrap().into_iter();
    let a = start(specs.next().unwrap(), "a", &ma);
    let b = start(specs.next().unwrap(), "b", &mb);
    let c = start(specs.next().unwrap(), "c", &mc);
    a.handle.dial(b.addr, b.id);
    a.handle.dial(c.addr, c.id);
    b.handle.dial(c.addr, c.id);
    for (x, y) in [(&a, &b), (&a, &c), (&b, &c)] {
        assert!(x.handle.wait_connected(y.id, true, WAIT).await);
    }
    let ok = tokio::time::timeout(WAIT, async {
        loop {
            let l = a.handle.layout();
            if l.as_ref().is_some_and(|l| l.screens.len() == 3)
                && l == b.handle.layout()
                && l == c.handle.layout()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "layouts converge");
    Three {
        a,
        b,
        c,
        ma,
        mb,
        mc,
    }
}

async fn peer_state(node: &TestNode, peer: &TestNode, want: DeviceState) -> bool {
    tokio::time::timeout(WAIT, async {
        loop {
            if node
                .handle
                .status()
                .peers
                .iter()
                .any(|p| p.id == peer.id && p.state == want)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// Pushes A's cursor against both outer edges for a while.
async fn push_both_edges(m: &MockPlatform) {
    for _ in 0..30 {
        m.capture.emit(CaptureEvent::Motion {
            x: 1918,
            y: 500,
            dx: 8,
            dy: 0,
        });
        m.capture.emit(CaptureEvent::Motion {
            x: 1,
            y: 500,
            dx: -8,
            dy: 0,
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_device_takes_no_cursor_and_no_clipboard() {
    let t = three().await;
    t.b.handle.set_state(StateCommand::Pause);
    assert!(
        peer_state(&t.a, &t.b, DeviceState::Paused).await,
        "A sees B paused"
    );
    assert!(peer_state(&t.c, &t.b, DeviceState::Paused).await);
    assert!(
        t.a.handle.is_connected(&t.b.id),
        "pausing keeps the connection"
    );

    push_both_edges(&t.ma).await;
    assert!(
        !t.mb
            .injector
            .log()
            .iter()
            .any(|e| matches!(e, Injected::MoveAbs { .. })),
        "the cursor never enters a paused device"
    );

    t.ma.clipboard
        .copy(&[(ClipFormat::TEXT, b"for c only")], false);
    assert!(t.mc.clipboard.wait_announced(WAIT, ClipFormat::TEXT).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        t.mb.clipboard.announce_count(),
        0,
        "B's clipboard is left alone"
    );
    t.mb.clipboard
        .copy(&[(ClipFormat::TEXT, b"from paused b")], false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        t.ma.clipboard.announce_count(),
        0,
        "a paused device does not share its clipboard"
    );

    t.b.handle.set_state(StateCommand::Resume);
    assert!(peer_state(&t.a, &t.b, DeviceState::Connected).await);
    t.ma.clipboard
        .copy(&[(ClipFormat::TEXT, b"for everyone")], false);
    assert!(
        t.mb.clipboard.wait_announced(WAIT, ClipFormat::TEXT).await,
        "back in after resuming"
    );
    // The emergency hotkey brings the cursor home first (it may sit on C).
    use bridge_protocol::input::HidUsage;
    for (k, down) in [
        (HidUsage::LEFT_CTRL, true),
        (HidUsage::LEFT_ALT, true),
        (HidUsage::LEFT_SHIFT, true),
        (HidUsage::ESCAPE, true),
        (HidUsage::ESCAPE, false),
        (HidUsage::LEFT_SHIFT, false),
        (HidUsage::LEFT_ALT, false),
        (HidUsage::LEFT_CTRL, false),
    ] {
        t.ma.capture.emit(CaptureEvent::Key {
            usage: k,
            down,
            repeat: false,
        });
    }
    // Towards B's side; if C lies in between, the cursor moves on through C to B.
    let l = t.a.handle.layout().unwrap();
    let x = |id| l.screens.iter().find(|s| s.device == id).unwrap().rect_mm.x;
    let dir = if x(t.b.id) > x(t.a.id) { 1 } else { -1 };
    let edge = if dir > 0 { 1918 } else { 1 };
    let entered = tokio::time::timeout(WAIT, async {
        loop {
            t.ma.capture.emit(CaptureEvent::Motion {
                x: edge,
                y: 500,
                dx: 40 * dir,
                dy: 0,
            });
            if t.mb
                .injector
                .log()
                .iter()
                .any(|e| matches!(e, Injected::MoveAbs { .. }))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(entered.is_ok(), "after resuming the cursor reaches B again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_disconnects_until_rejoining() {
    let t = three().await;
    t.b.handle.set_state(StateCommand::Leave);
    assert!(
        t.a.handle.wait_connected(t.b.id, false, WAIT).await,
        "A loses B"
    );
    assert!(
        t.c.handle.wait_connected(t.b.id, false, WAIT).await,
        "C loses B"
    );
    assert_eq!(t.b.handle.status().state, DeviceState::Left);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !t.a.handle.is_connected(&t.b.id),
        "nobody dials a device that left"
    );
    assert!(!t.c.handle.is_connected(&t.b.id));
    assert!(t.a.handle.is_connected(&t.c.id), "the others stay together");
    let l = t.a.handle.layout().unwrap();
    assert_eq!(
        l.screens.len(),
        3,
        "B's screen stays in the layout; the cursor skips it"
    );

    t.b.handle.set_state(StateCommand::Rejoin);
    assert!(
        t.a.handle.wait_connected(t.b.id, true, WAIT).await,
        "B dials back in"
    );
    assert!(t.c.handle.wait_connected(t.b.id, true, WAIT).await);
    assert_eq!(t.b.handle.status().state, DeviceState::Connected);
    t.ma.clipboard
        .copy(&[(ClipFormat::TEXT, b"welcome back")], false);
    assert!(t.mb.clipboard.wait_announced(WAIT, ClipFormat::TEXT).await);
}
