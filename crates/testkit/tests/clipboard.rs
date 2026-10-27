//! Integration: the shared clipboard between full nodes with mock clipboards.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use bridge_feature_clipboard::{ClipboardConfig, ClipboardFeature};
use bridge_platform::ClipFormat;
use bridge_platform::mock::{MockClipboard, MockPlatform};
use bridge_protocol::FeatureId;
use bridge_testkit::{Network, NodeSpec, TestNode, mutually_trusting};

const WAIT: Duration = Duration::from_secs(10);

fn start(spec: NodeSpec, name: &str, mock: &MockPlatform) -> TestNode {
    let clipboard = mock.clipboard.clone();
    spec.start(name, move |b| {
        let post = b.local_sender(FeatureId::CLIPBOARD).into_post();
        let feature = ClipboardFeature::new(clipboard, ClipboardConfig::default(), post).unwrap();
        b.feature(feature).unwrap()
    })
}

async fn pair() -> (MockPlatform, MockPlatform, TestNode, TestNode) {
    let net = Network::new(301);
    let (ma, mb) = (MockPlatform::new(), MockPlatform::new());
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = start(specs.next().unwrap(), "desk", &ma);
    let b = start(specs.next().unwrap(), "laptop", &mb);
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(b.handle.wait_connected(a.id, true, WAIT).await);
    (ma, mb, a, b)
}

/// Pastes on a blocking thread, as an OS clipboard thread would.
async fn paste(c: &Arc<MockClipboard>, format: ClipFormat) -> Option<Vec<u8>> {
    let c = c.clone();
    tokio::task::spawn_blocking(move || c.paste(format))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copy_on_one_device_pastes_on_the_other_lazily() {
    let (ma, mb, _a, _b) = pair().await;
    let image: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
    let long_text = "строка\n".repeat(30_000);
    ma.clipboard.copy(
        &[
            (ClipFormat::TEXT, long_text.as_bytes()),
            (ClipFormat::HTML, b"<b>hi</b>"),
            (ClipFormat::PNG, &image),
        ],
        false,
    );
    assert!(mb.clipboard.wait_announced(WAIT, ClipFormat::PNG).await);
    assert_eq!(
        mb.clipboard.announced_formats().unwrap(),
        vec![ClipFormat::TEXT, ClipFormat::HTML, ClipFormat::PNG]
    );
    assert_eq!(
        paste(&mb.clipboard, ClipFormat::HTML).await.unwrap(),
        b"<b>hi</b>"
    );
    let text = paste(&mb.clipboard, ClipFormat::TEXT).await.unwrap();
    assert_eq!(
        text.len(),
        long_text.len(),
        "over the inline limit, fetched on paste"
    );
    assert_eq!(text, long_text.as_bytes());
    assert_eq!(
        paste(&mb.clipboard, ClipFormat::PNG).await.unwrap(),
        image,
        "a 1.5 MB image arrives in chunks"
    );
    assert_eq!(
        paste(&mb.clipboard, ClipFormat::RTF).await,
        None,
        "not offered"
    );

    // B's own takeover is not offered back to A.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(ma.clipboard.announce_count(), 0, "no echo");

    // Copying on B now goes the other way.
    mb.clipboard.copy(&[(ClipFormat::TEXT, b"back")], false);
    assert!(ma.clipboard.wait_announced(WAIT, ClipFormat::TEXT).await);
    assert_eq!(
        paste(&ma.clipboard, ClipFormat::TEXT).await.unwrap(),
        b"back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concealed_copies_and_stale_offers_are_not_shared() {
    let (ma, mb, _a, _b) = pair().await;
    ma.clipboard.copy(&[(ClipFormat::TEXT, b"one")], false);
    assert!(mb.clipboard.wait_announced(WAIT, ClipFormat::TEXT).await);
    let image = vec![1u8; 600_000];
    ma.clipboard.copy(&[(ClipFormat::PNG, &image)], false);
    assert!(mb.clipboard.wait_announced(WAIT, ClipFormat::PNG).await);
    let before = mb.clipboard.announce_count();

    ma.clipboard.copy(&[(ClipFormat::TEXT, b"hunter2")], true);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        mb.clipboard.announce_count(),
        before,
        "a password stays on A"
    );
    assert_eq!(
        paste(&mb.clipboard, ClipFormat::PNG).await,
        None,
        "A's clipboard changed since the offer: the old image is gone"
    );
}
