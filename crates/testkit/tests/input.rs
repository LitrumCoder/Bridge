//! Integration: shared cursor and keyboard between full nodes with mock platforms.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use bridge_core::input::CaptureEvent;
use bridge_feature_input::{InputConfig, InputFeature, Wake};
use bridge_platform::ScreenInfo;
use bridge_platform::mock::{Injected, MockPlatform};
use bridge_protocol::FeatureId;
use bridge_protocol::input::{HidUsage, MouseButton};
use bridge_testkit::{Network, NodeSpec, TestNode, mutually_trusting};

const WAIT: Duration = Duration::from_secs(10);

fn start(spec: NodeSpec, name: &str, mock: &MockPlatform) -> TestNode {
    let id = spec.identity.id();
    let platform = mock.platform();
    let screens = mock.screens.screens();
    spec.start(name, move |b| {
        let local = b.local_sender(FeatureId::INPUT);
        let feature = InputFeature::new(
            id,
            platform,
            InputConfig::default(),
            Arc::new(move || local.post(Wake)),
        )
        .unwrap();
        b.screens(screens).feature(feature).unwrap()
    })
}

async fn both_in_layout(a: &TestNode, b: &TestNode) {
    let ok = tokio::time::timeout(WAIT, async {
        loop {
            let la = a.handle.layout();
            if la.as_ref().is_some_and(|l| l.screens.len() == 2) && la == b.handle.layout() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "layouts converge");
}

/// +1 if B's screen lies to the right of A's in the shared layout, -1 if to the left
/// (which device ends up where depends on whose initial layout won).
fn direction(a: &TestNode, b: &TestNode) -> i32 {
    let l = a.handle.layout().unwrap();
    let x = |id| l.screens.iter().find(|s| s.device == id).unwrap().rect_mm.x;
    if x(b.id) > x(a.id) { 1 } else { -1 }
}

/// Pushes the cursor across A's edge towards B until B starts receiving it.
async fn cross(src: &MockPlatform, dst: &MockPlatform, dir: i32) {
    let x = if dir > 0 { 1918 } else { 1 };
    let ok = tokio::time::timeout(WAIT, async {
        loop {
            src.capture.emit(CaptureEvent::Motion {
                x,
                y: 500,
                dx: 6 * dir,
                dy: 0,
            });
            if dst
                .injector
                .wait_for(Duration::from_millis(100), |l| {
                    l.iter().any(|e| matches!(e, Injected::MoveAbs { .. }))
                })
                .await
            {
                return;
            }
        }
    })
    .await;
    assert!(
        ok.is_ok(),
        "cursor reaches the other device (source grabbed: {})",
        src.capture.is_grabbed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_and_keys_move_to_the_peer_and_back() {
    let net = Network::new(201);
    let (ma, mb) = (MockPlatform::new(), MockPlatform::new());
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = start(specs.next().unwrap(), "desk", &ma);
    let b = start(specs.next().unwrap(), "laptop", &mb);
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    both_in_layout(&a, &b).await;
    let dir = direction(&a, &b);

    cross(&ma, &mb, dir).await;
    assert!(
        ma.capture.is_grabbed(),
        "A grabs its input while the cursor is away"
    );
    let entry = mb.injector.log().into_iter().find_map(|e| match e {
        Injected::MoveAbs { x, y, .. } => Some((x, y)),
        _ => None,
    });
    let (x, y) = entry.unwrap();
    let near_edge = if dir > 0 { x < 20 } else { x > 1900 };
    assert!(
        near_edge && (490..=510).contains(&y),
        "enters at the facing edge, same height: {x},{y}"
    );

    ma.capture.emit(CaptureEvent::Key {
        usage: HidUsage::A,
        down: true,
        repeat: false,
    });
    ma.capture.emit(CaptureEvent::Wheel { dx: 0, dy: -120 });
    ma.capture.emit(CaptureEvent::Motion {
        x: 0,
        y: 0,
        dx: 300 * dir,
        dy: 40,
    });
    assert!(
        mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Key {
                usage: HidUsage::A,
                down: true
            }))
            .await
    );
    assert!(
        mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Wheel { dx: 0, dy: -120 }))
            .await
    );
    ma.capture.emit(CaptureEvent::Key {
        usage: HidUsage::A,
        down: false,
        repeat: false,
    });
    assert!(
        mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Key {
                usage: HidUsage::A,
                down: false
            }))
            .await
    );

    // Back to A in small steps, as a hand would move the mouse.
    let back = tokio::time::timeout(WAIT, async {
        while ma.capture.is_grabbed() {
            ma.capture.emit(CaptureEvent::Motion {
                x: 0,
                y: 0,
                dx: -40 * dir,
                dy: 0,
            });
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(back.is_ok(), "A releases the grab");
    let warped_to_edge = ma
        .capture
        .warps()
        .iter()
        .any(|(x, _)| if dir > 0 { *x > 1800 } else { *x < 120 });
    assert!(
        warped_to_edge,
        "cursor reappears at A's edge facing B: {:?}",
        ma.capture.warps()
    );
    assert!(mb.injector.held_keys().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_loss_releases_held_keys_on_the_target() {
    let net = Network::new(202);
    let (ma, mb) = (MockPlatform::new(), MockPlatform::new());
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = start(specs.next().unwrap(), "desk", &ma);
    let b = start(specs.next().unwrap(), "laptop", &mb);
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    both_in_layout(&a, &b).await;
    cross(&ma, &mb, direction(&a, &b)).await;

    ma.capture.emit(CaptureEvent::Key {
        usage: HidUsage::LEFT_SHIFT,
        down: true,
        repeat: false,
    });
    ma.capture.emit(CaptureEvent::Button {
        button: MouseButton::Left,
        down: true,
    });
    assert!(
        mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Button {
                button: MouseButton::Left,
                down: true
            }))
            .await
    );
    assert_eq!(mb.injector.held_keys(), vec![HidUsage::LEFT_SHIFT]);

    net.partition(a.addr, b.addr);
    assert!(
        mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Key {
                usage: HidUsage::LEFT_SHIFT,
                down: false
            }) && l.contains(&Injected::Button {
                button: MouseButton::Left,
                down: false
            }))
            .await,
        "the target releases keys and buttons by itself when the source disappears"
    );
    assert!(mb.injector.held_keys().is_empty());

    let home = tokio::time::timeout(WAIT, async {
        while ma.capture.is_grabbed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        home.is_ok(),
        "the source gets its cursor back when the target is lost"
    );
    // Releasing the physical keys afterwards reaches nobody and breaks nothing.
    ma.capture.emit(CaptureEvent::Key {
        usage: HidUsage::LEFT_SHIFT,
        down: false,
        repeat: false,
    });
    ma.capture.emit(CaptureEvent::Button {
        button: MouseButton::Left,
        down: false,
    });
    net.heal(a.addr, b.addr);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(mb.injector.held_keys().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_permission_denied_keeps_the_edge_closed() {
    let net = Network::new(203);
    let (ma, mb) = (MockPlatform::new(), MockPlatform::new());
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = start(specs.next().unwrap(), "desk", &ma);
    let b = start(specs.next().unwrap(), "laptop", &mb);
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    both_in_layout(&a, &b).await;
    // B no longer lets A control it.
    b.handle
        .set_permissions(a.id, vec![bridge_protocol::PermissionId::FILES]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dir = direction(&a, &b);
    for _ in 0..20 {
        ma.capture.emit(CaptureEvent::Motion {
            x: if dir > 0 { 1918 } else { 1 },
            y: 500,
            dx: 6 * dir,
            dy: 0,
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!ma.capture.is_grabbed());
    assert!(mb.injector.log().is_empty(), "nothing reaches B");
}
