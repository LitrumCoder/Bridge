//! Integration: MIDI forwarding between full nodes with mock ports (ADR-0009).

#![allow(clippy::unwrap_used, clippy::print_stderr)]

use std::sync::Arc;
use std::time::Duration;

use bridge_feature_midi::{MidiConfig, MidiFeature, MidiHandle};
use bridge_platform::mock::MockMidi;
use bridge_protocol::FeatureId;
use bridge_testkit::midi_pair::{INSTRUMENT, MidiPair};
use bridge_testkit::{LinkConfig, Network, NodeSpec, TestNode, mutually_trusting};

const WAIT: Duration = Duration::from_secs(20);

fn start(spec: NodeSpec, name: &str, midi: Arc<MockMidi>) -> (TestNode, MidiHandle) {
    let mut handle = None;
    let node = spec.start(name, |b| {
        let post = b.local_sender(FeatureId::MIDI).into_post();
        let (f, h) = MidiFeature::new(midi, MidiConfig::new(name), post);
        handle = Some(h);
        b.feature(f).unwrap()
    });
    (node, handle.unwrap())
}

struct Net {
    net: Network,
    a: TestNode,
    b: TestNode,
    ma: Arc<MockMidi>,
    mb: Arc<MockMidi>,
    port: String,
}

async fn over(link: LinkConfig) -> Net {
    bridge_testkit::fine_timers();
    let net = Network::new(601);
    net.set_default_link(link);
    let ma = Arc::new(MockMidi::with_instrument(INSTRUMENT));
    let mb = Arc::new(MockMidi::default());
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let (a, _) = start(specs.next().unwrap(), "studio", ma.clone());
    let (b, _) = start(specs.next().unwrap(), "laptop", mb.clone());
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    let port = format!("{INSTRUMENT} via studio");
    let ok = tokio::time::timeout(WAIT, async {
        while !mb.virtual_ports().contains(&port) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "B offers the instrument to its applications");
    Net {
        net,
        a,
        b,
        ma,
        mb,
        port,
    }
}

fn sequence(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| match i % 10 {
            0 => {
                let mut sx = vec![0xF0, 0x7E];
                sx.extend((0..1000).map(|j| (j % 128) as u8));
                sx.push(0xF7);
                sx
            }
            1 => vec![0xF8],
            2..=5 => vec![0x90, (i % 128) as u8, 100],
            6 => vec![0xB0, 1, (i % 128) as u8],
            _ => vec![0x80, (i % 128) as u8, 0],
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_event_arrives_in_order_through_loss_and_jitter() {
    let n = over(LinkConfig {
        latency: Duration::from_millis(4),
        jitter: Duration::from_millis(3),
        loss: 0.05,
        ..LinkConfig::default()
    })
    .await;
    // Let clock sync collect samples so events are scheduled on the sender's timing.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let msgs = sequence(600);
    for m in &msgs {
        assert!(n.ma.play(INSTRUMENT, m));
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        n.mb.wait_written(&n.port, msgs.len(), WAIT).await,
        "all {} events arrive",
        msgs.len()
    );
    let got: Vec<Vec<u8>> = n.mb.written(&n.port).into_iter().map(|(_, m)| m).collect();
    assert_eq!(got, msgs, "nothing lost, nothing reordered, SysEx intact");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daw_on_the_receiver_plays_the_instrument() {
    let n = over(LinkConfig::default()).await;
    let ok = tokio::time::timeout(WAIT, async {
        while !n.mb.daw_play(&n.port, &[0x91, 60, 90]) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "B also has a destination for the instrument");
    assert!(n.ma.wait_written(INSTRUMENT, 1, WAIT).await);
    assert_eq!(n.ma.written(INSTRUMENT)[0].1, vec![0x91, 60, 90]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_notes_are_released_when_the_peer_is_lost() {
    let n = over(LinkConfig::default()).await;
    assert!(n.ma.play(INSTRUMENT, &[0x90, 64, 100]));
    assert!(n.ma.play(INSTRUMENT, &[0xB0, 64, 127]));
    assert!(n.mb.wait_written(&n.port, 2, WAIT).await);
    n.net.partition(n.a.addr, n.b.addr);
    assert!(
        n.mb.wait_written(&n.port, 4, WAIT).await,
        "note-off and pedal up after the loss"
    );
    let tail: Vec<Vec<u8>> = n.mb.written(&n.port)[2..]
        .iter()
        .map(|(_, m)| m.clone())
        .collect();
    assert!(tail.contains(&vec![0x80, 64, 0]), "{tail:?}");
    assert!(tail.contains(&vec![0xB0, 64, 0]), "{tail:?}");
    assert!(
        n.mb.virtual_ports().contains(&n.port),
        "the port stays for a short outage"
    );
    let _ = &n.b;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latency_over_loopback_stays_within_the_target() {
    let pair = MidiPair::connected().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut v = Vec::new();
    for i in 0..100 {
        v.push(pair.note_latency(36 + (i % 48) as u8).await);
    }
    v.sort();
    let (p50, p99) = (v[50], v[98]);
    let stats = pair.hb.streams();
    eprintln!(
        "MIDI instrument → DAW over loopback (3 ms buffer): p50 {p50:?}, p99 {p99:?}; {stats:?}"
    );
    // ADR-0009 wants under 5 ms (3 ms of it is the buffer); that target is checked by the
    // criterion bench. Shared CI runners (macOS VMs especially) only get a sanity bound here.
    assert!(p50 < Duration::from_millis(10), "p50 {p50:?}");
    assert!(p99 < Duration::from_millis(30), "p99 {p99:?}");
    pair.shutdown().await;
}
