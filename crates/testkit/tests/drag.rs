//! Integration: dragging files across the screen edge (ADR-0008) between full nodes
//! with mock platforms — the input router hands the button over, the files go to the
//! receiver's staging folder while its OS drag runs, and the drop ends both sides.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bridge_core::input::{CaptureEvent, RouterEvent};
use bridge_feature_files::{
    CursorEvent, DragWiring, FilesConfig, FilesFeature, FilesHandle, TransferState,
};
use bridge_feature_input::{InputConfig, InputFeature, Wake};
use bridge_platform::ScreenInfo;
use bridge_platform::mock::{Injected, MockPlatform};
use bridge_protocol::FeatureId;
use bridge_protocol::input::MouseButton;
use bridge_testkit::{Network, NodeSpec, TestNode, mutually_trusting};

const WAIT: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bridge-drag-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn start(spec: NodeSpec, name: &str, mock: &MockPlatform, dir: PathBuf) -> (TestNode, FilesHandle) {
    bridge_testkit::fine_timers();
    let id = spec.identity.id();
    let platform = mock.platform();
    let screens = mock.screens.screens();
    let (source, target) = (mock.drag_source.clone(), mock.drop_target.clone());
    let mut handle = None;
    let node = spec.start(name, |b| {
        let local = b.local_sender(FeatureId::INPUT);
        let input = InputFeature::new(
            id,
            platform,
            InputConfig::default(),
            Arc::new(move || local.post(Wake)),
        )
        .unwrap();
        let mut cfg = FilesConfig::new(dir.join("downloads"));
        cfg.staging_dir = dir.join("staging");
        let (link, incoming) = b.bulk();
        let (files, fh) = FilesFeature::new(
            cfg,
            link,
            incoming,
            b.local_sender(FeatureId::FILES).into_post(),
        );
        let control = input.control();
        let files = files.with_drag(DragWiring {
            source,
            target: Some(target),
            set_drag_active: Arc::new(move |on| control.set_drag_active(on)),
        });
        let events = fh.clone();
        let input = input.on_router_event(Arc::new(move |e| match e {
            RouterEvent::Entered { peer } => events.cursor(CursorEvent::Entered(*peer)),
            RouterEvent::Returned { .. } => events.cursor(CursorEvent::Returned),
            _ => {}
        }));
        handle = Some(fh);
        b.screens(screens)
            .feature(input)
            .unwrap()
            .feature(files)
            .unwrap()
    });
    (node, handle.unwrap())
}

struct Setup {
    ma: MockPlatform,
    mb: MockPlatform,
    fa: FilesHandle,
    fb: FilesHandle,
    dir: i32,
    _nodes: (TestNode, TestNode),
}

async fn setup(name: &str) -> Setup {
    let net = Network::new(501);
    let (ma, mb) = (MockPlatform::new(), MockPlatform::new());
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let (a, fa) = start(
        specs.next().unwrap(),
        "desk",
        &ma,
        scratch(&format!("{name}-a")),
    );
    let (b, fb) = start(
        specs.next().unwrap(),
        "laptop",
        &mb,
        scratch(&format!("{name}-b")),
    );
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
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
    let l = a.handle.layout().unwrap();
    let x = |id| l.screens.iter().find(|s| s.device == id).unwrap().rect_mm.x;
    let dir = if x(b.id) > x(a.id) { 1 } else { -1 };
    Setup {
        ma,
        mb,
        fa,
        fb,
        dir,
        _nodes: (a, b),
    }
}

fn left(down: bool) -> CaptureEvent {
    CaptureEvent::Button {
        button: MouseButton::Left,
        down,
    }
}

/// The user grabs files, drags them to the edge and on across it.
async fn drag_across(s: &Setup, paths: &[PathBuf]) {
    s.ma.capture.emit(left(true));
    s.ma.drag_source.drag_to_edge(paths);
    let x = if s.dir > 0 { 1918 } else { 1 };
    let crossed = tokio::time::timeout(WAIT, async {
        loop {
            s.ma.capture.emit(CaptureEvent::Motion {
                x,
                y: 500,
                dx: 6 * s.dir,
                dy: 0,
            });
            if s.mb
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
        crossed.is_ok(),
        "the cursor crosses with the button held once the drag is known"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_dragged_across_the_edge_drop_on_the_other_device() {
    let s = setup("drop").await;
    let src = scratch("drop-src");
    std::fs::create_dir_all(src.join("Сэмплы")).unwrap();
    let kick: Vec<u8> = (0..3_000_000u32).map(|i| (i % 249) as u8).collect();
    std::fs::write(src.join("Сэмплы/kick.wav"), &kick).unwrap();
    std::fs::write(src.join("notes.txt"), b"bpm 128").unwrap();
    drag_across(&s, &[src.join("Сэмплы"), src.join("notes.txt")]).await;

    assert!(
        s.mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Button {
                button: MouseButton::Left,
                down: true
            }))
            .await,
        "B's OS gets the button down for its drag session"
    );
    let items =
        s.mb.drop_target
            .wait_session(WAIT)
            .await
            .expect("an OS drag starts on B");
    assert_eq!(items.len(), 2, "top-level items: {items:?}");
    assert!(items[0].is_dir && items[0].path.ends_with("Сэмплы"));
    assert!(
        s.ma.drag_source.holds() >= 1,
        "A keeps its local drag loop over Bridge"
    );

    // Releasing the button on A drops on B (and ends A's loop locally).
    assert_eq!(
        s.ma.capture.emit(left(false)),
        bridge_core::input::Decision::Pass
    );
    assert!(
        s.mb.injector
            .wait_for(WAIT, |l| l.contains(&Injected::Button {
                button: MouseButton::Left,
                down: false
            }))
            .await
    );
    let target = s.mb.drop_target.clone();
    let complete = tokio::task::spawn_blocking(move || target.drop_and_wait(true, WAIT))
        .await
        .unwrap();
    assert!(complete, "the drop target got complete files");
    assert_eq!(std::fs::read(items[0].path.join("kick.wav")).unwrap(), kick);
    assert_eq!(std::fs::read(&items[1].path).unwrap(), b"bpm 128");

    let finished = tokio::time::timeout(WAIT, async {
        while s.ma.drag_source.finishes() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(finished.is_ok(), "A ends its drag after B's drop");
    let id = s.fa.transfers()[0].id;
    assert_eq!(
        s.fa.wait(id, WAIT).await.unwrap().state,
        TransferState::Done
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_drop_cancels_the_transfer() {
    let s = setup("cancel").await;
    let src = scratch("cancel-src");
    std::fs::write(src.join("big.bin"), vec![7u8; 8_000_000]).unwrap();
    drag_across(&s, &[src.join("big.bin")]).await;
    let items = s.mb.drop_target.wait_session(WAIT).await.unwrap();
    let target = s.mb.drop_target.clone();
    tokio::task::spawn_blocking(move || target.drop_and_wait(false, WAIT))
        .await
        .unwrap();
    let id = s.fb.transfers()[0].id;
    assert_eq!(
        s.fb.wait(id, WAIT).await.unwrap().state,
        TransferState::Cancelled
    );
    assert_eq!(
        s.fa.wait(id, WAIT).await.unwrap().state,
        TransferState::Cancelled
    );
    assert!(s.ma.drag_source.finishes() >= 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!items[0].path.exists(), "nothing left in staging");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drag_that_turns_back_is_not_sent() {
    let s = setup("away").await;
    let src = scratch("away-src");
    std::fs::write(src.join("x.txt"), b"x").unwrap();
    s.ma.capture.emit(left(true));
    s.ma.drag_source.drag_to_edge(&[src.join("x.txt")]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.ma.drag_source.drag_away();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let x = if s.dir > 0 { 1918 } else { 1 };
    for _ in 0..20 {
        s.ma.capture.emit(CaptureEvent::Motion {
            x,
            y: 500,
            dx: 6 * s.dir,
            dy: 0,
        });
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        s.mb.injector.log().is_empty(),
        "with the drag gone, the held button blocks the edge again"
    );
    assert!(s.fa.transfers().is_empty());
    s.ma.capture.emit(left(false));
}
