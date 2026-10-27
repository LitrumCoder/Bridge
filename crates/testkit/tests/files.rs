//! Integration: file transfers between full nodes over the simulated network (QUIC
//! interactive + bulk connections): trees, mtimes, conflicts, resume, cancel, a source
//! that changes mid-transfer, permissions.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bridge_feature_files::{FilesConfig, FilesFeature, FilesHandle, TransferState};
use bridge_protocol::{FeatureId, PermissionId};
use bridge_testkit::{LinkConfig, Network, NodeSpec, TestNode, mutually_trusting};

const WAIT: Duration = Duration::from_secs(60);

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bridge-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn start(spec: NodeSpec, name: &str, config: FilesConfig) -> (TestNode, FilesHandle) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    // Coarse Windows timers would look like queuing delay to the bulk controller.
    #[cfg(windows)]
    // SAFETY: plain call; the period is restored when the process exits.
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    let mut handle = None;
    let node = spec.start(name, |b| {
        let post = b.local_sender(FeatureId::FILES).into_post();
        let (link, incoming) = b.bulk();
        let (feature, h) = FilesFeature::new(config, link, incoming, post);
        handle = Some(h);
        b.feature(feature).unwrap()
    });
    (node, handle.unwrap())
}

struct Pair {
    net: Network,
    a: TestNode,
    b: TestNode,
    fa: FilesHandle,
    fb: FilesHandle,
    inbox: PathBuf,
}

async fn pair(
    name: &str,
    link: Option<LinkConfig>,
    receiver: impl FnOnce(&mut FilesConfig),
) -> Pair {
    let net = Network::new(401);
    if let Some(l) = link {
        net.set_default_link(l);
    }
    let inbox = scratch(&format!("{name}-inbox"));
    let mut cb = FilesConfig::new(inbox.clone());
    receiver(&mut cb);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let (a, fa) = start(
        specs.next().unwrap(),
        "desk",
        FilesConfig::new(scratch(&format!("{name}-a"))),
    );
    let (b, fb) = start(specs.next().unwrap(), "laptop", cb);
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(b.handle.wait_connected(a.id, true, WAIT).await);
    Pair {
        net,
        a,
        b,
        fa,
        fb,
        inbox,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn set_mtime(p: &Path, secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().contains(".bridgepart") {
                out.push(p);
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_arrives_intact_with_mtimes_and_conflict_names() {
    let p = pair("tree", None, |_| {}).await;
    let src = scratch("tree-src");
    let root = src.join("Проект");
    std::fs::create_dir_all(root.join("docs/пусто")).unwrap();
    let big = pattern(5_000_000, 3);
    std::fs::write(root.join("data.bin"), &big).unwrap();
    std::fs::write(root.join("docs/readme.txt"), b"hello").unwrap();
    std::fs::write(root.join("empty.txt"), b"").unwrap();
    set_mtime(&root.join("docs/readme.txt"), 1_600_000_000);
    std::fs::write(src.join("note.txt"), b"new").unwrap();
    // Already in the receiver's folder: the new copy must not overwrite it.
    std::fs::write(p.inbox.join("note.txt"), b"old").unwrap();

    let id =
        p.fa.send(p.b.id, vec![root.clone(), src.join("note.txt")])
            .await
            .unwrap();
    let sent = p.fa.wait(id, WAIT).await.unwrap();
    let got = p.fb.wait(id, WAIT).await.unwrap();
    assert_eq!(sent.state, TransferState::Done);
    assert_eq!(got.state, TransferState::Done);
    assert_eq!(got.items, 7);
    assert_eq!(got.names, vec!["Проект", "note.txt"]);

    let dst = p.inbox.join("Проект");
    assert_eq!(std::fs::read(dst.join("data.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read(dst.join("docs/readme.txt")).unwrap(),
        b"hello"
    );
    assert!(dst.join("docs/пусто").is_dir(), "empty folders are created");
    assert_eq!(std::fs::read(dst.join("empty.txt")).unwrap(), b"");
    let mtime = std::fs::metadata(dst.join("docs/readme.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)
    );
    assert_eq!(std::fs::read(p.inbox.join("note.txt")).unwrap(), b"old");
    assert_eq!(std::fs::read(p.inbox.join("note (1).txt")).unwrap(), b"new");
    assert!(leftovers(&p.inbox).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_transfer_resumes_where_it_stopped() {
    let link = LinkConfig {
        latency: Duration::from_millis(2),
        bandwidth_bps: Some(40_000_000),
        queue_bytes: 1 << 20,
        ..LinkConfig::default()
    };
    let p = pair("resume", Some(link), |c| c.stall = Duration::from_secs(2)).await;
    let src = scratch("resume-src");
    let data = pattern(24_000_000, 9);
    std::fs::write(src.join("big.iso"), &data).unwrap();
    let id = p.fa.send(p.b.id, vec![src.join("big.iso")]).await.unwrap();

    let waited = tokio::time::timeout(WAIT, async {
        while p.fb.get(id).is_none_or(|s| s.done_bytes < 6_000_000) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the transfer gets going");
    p.net.partition(p.a.addr, p.b.addr);
    assert!(
        p.a.handle.wait_connected(p.b.id, false, WAIT).await,
        "the link is cut"
    );
    p.net.heal(p.a.addr, p.b.addr);
    p.a.handle.dial(p.b.addr, p.b.id);
    assert!(p.a.handle.wait_connected(p.b.id, true, WAIT).await);

    let got = p.fb.wait(id, WAIT).await.unwrap();
    assert_eq!(got.state, TransferState::Done);
    assert!(
        got.resumed_bytes >= 5_000_000,
        "resumed from {} bytes",
        got.resumed_bytes
    );
    assert_eq!(
        std::fs::read(p.inbox.join("big.iso")).unwrap(),
        data,
        "BLAKE3-checked content"
    );
    assert!(leftovers(&p.inbox).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_removes_partial_files_on_the_receiver() {
    let link = LinkConfig {
        bandwidth_bps: Some(20_000_000),
        ..LinkConfig::default()
    };
    let p = pair("cancel", Some(link), |_| {}).await;
    let src = scratch("cancel-src");
    std::fs::write(src.join("huge.bin"), pattern(30_000_000, 1)).unwrap();
    let id = p.fa.send(p.b.id, vec![src.join("huge.bin")]).await.unwrap();
    tokio::time::timeout(WAIT, async {
        while p.fb.get(id).is_none_or(|s| s.done_bytes < 1_000_000) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    p.fa.cancel(id);
    assert_eq!(
        p.fa.wait(id, WAIT).await.unwrap().state,
        TransferState::Cancelled
    );
    assert_eq!(
        p.fb.wait(id, WAIT).await.unwrap().state,
        TransferState::Cancelled
    );
    // The stream task may still be writing its last block when the cancel arrives.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(leftovers(&p.inbox).is_empty(), "{:?}", leftovers(&p.inbox));
    assert!(!p.inbox.join("huge.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_that_changes_before_it_is_sent_is_fetched_again() {
    let link = LinkConfig {
        bandwidth_bps: Some(40_000_000),
        ..LinkConfig::default()
    };
    let p = pair("changed", Some(link), |c| c.parallel = 1).await;
    let src = scratch("changed-src");
    std::fs::write(src.join("1-first.bin"), pattern(15_000_000, 2)).unwrap();
    std::fs::write(src.join("2-second.txt"), b"version 1").unwrap();
    let id =
        p.fa.send(
            p.b.id,
            vec![src.join("1-first.bin"), src.join("2-second.txt")],
        )
        .await
        .unwrap();
    // While the first file is on the way, the second one is edited.
    std::fs::write(src.join("2-second.txt"), b"version 2, longer").unwrap();
    let got = p.fb.wait(id, WAIT).await.unwrap();
    assert_eq!(got.state, TransferState::Done);
    assert_eq!(
        std::fs::read(p.inbox.join("2-second.txt")).unwrap(),
        b"version 2, longer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_that_does_not_accept_files_gets_none() {
    let p = pair("denied", None, |_| {}).await;
    assert!(
        p.b.handle
            .set_permissions(p.a.id, vec![PermissionId::INPUT, PermissionId::CLIPBOARD])
    );
    let src = scratch("denied-src");
    std::fs::write(src.join("x.txt"), b"x").unwrap();
    let refused = tokio::time::timeout(WAIT, async {
        loop {
            if p.fa.send(p.b.id, vec![src.join("x.txt")]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        refused.is_ok(),
        "A learns that B does not take files from it"
    );
    assert!(!p.inbox.join("x.txt").exists());
}
