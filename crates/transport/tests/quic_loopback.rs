#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bridge_core::ConnectionKind;
use bridge_protocol::{CloseCode, DeviceId};
use bridge_transport::*;
use bytes::Bytes;

#[derive(Default)]
struct Trust {
    ids: Mutex<HashSet<DeviceId>>,
}

impl TrustPolicy for Trust {
    fn is_trusted(&self, id: &DeviceId) -> bool {
        self.ids.lock().unwrap().contains(id)
    }
}

fn endpoint() -> (QuicTransport, Arc<Trust>, DeviceId) {
    let identity = DeviceIdentity::generate().unwrap();
    let id = identity.id();
    let trust = Arc::new(Trust::default());
    let t = QuicTransport::bind(
        "127.0.0.1:0".parse().unwrap(),
        identity,
        trust.clone(),
        QuicOptions::default(),
    )
    .unwrap();
    (t, trust, id)
}

fn addr(t: &QuicTransport) -> SocketAddr {
    match t.local_addr().unwrap() {
        PeerAddr::Ip(a) => a,
    }
}

#[tokio::test]
async fn trusted_peers_exchange_streams_and_datagrams() {
    let (a, trust_a, id_a) = endpoint();
    let (b, trust_b, id_b) = endpoint();
    trust_a.ids.lock().unwrap().insert(id_b);
    trust_b.ids.lock().unwrap().insert(id_a);

    let b_addr = addr(&b);
    let server = tokio::spawn(async move {
        let conn = b.accept().await.unwrap().unwrap();
        assert_eq!(conn.peer(), id_a);
        assert_eq!(conn.kind(), ConnectionKind::Interactive);
        let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
        let mut buf = [0u8; 5];
        let mut got = 0;
        while got < 5 {
            got += rx.read(&mut buf[got..]).await.unwrap().unwrap();
        }
        tx.write_all(&buf).await.unwrap();
        tx.finish().unwrap();
        let d = conn.recv_datagram().await.unwrap();
        assert_eq!(&d[..], b"motion");
        let info = conn.closed().await;
        assert_eq!(info.by, ClosedBy::Peer);
        assert_eq!(info.code, Some(CloseCode::LEAVING));
        b
    });

    let conn = a
        .connect(
            PeerAddr::Ip(b_addr),
            ExpectedPeer::Device(id_b),
            ConnectionKind::Interactive,
        )
        .await
        .unwrap();
    assert_eq!(conn.peer(), id_b);
    let (mut tx, mut rx) = conn.open_bi(100).await.unwrap();
    tx.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    let mut got = 0;
    while got < 5 {
        got += rx.read(&mut buf[got..]).await.unwrap().unwrap();
    }
    assert_eq!(&buf, b"hello");
    assert!(conn.max_datagram_size().unwrap() > 100);
    conn.send_datagram(Bytes::from_static(b"motion")).unwrap();
    assert!(conn.stats().rtt > std::time::Duration::ZERO);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    conn.close(CloseCode::LEAVING, "bye");
    let local = conn.closed().await;
    assert_eq!(local.by, ClosedBy::Local);
    assert_eq!(local.code, Some(CloseCode::LEAVING));
    server.await.unwrap();
}

#[tokio::test]
async fn untrusted_client_is_refused_during_tls() {
    let (a, _trust_a, _id_a) = endpoint();
    let (b, trust_b, id_b) = endpoint();
    let _ = trust_b; // B trusts nobody.
    let b_addr = addr(&b);
    let server = tokio::spawn(async move {
        let r = b.accept().await.unwrap();
        assert!(r.is_err(), "handshake must fail on the server too");
    });
    // The refusal may come during connect or right after it.
    match a
        .connect(
            PeerAddr::Ip(b_addr),
            ExpectedPeer::Device(id_b),
            ConnectionKind::Interactive,
        )
        .await
    {
        Err(err) => assert!(matches!(err, TransportError::NotPaired), "{err:?}"),
        Ok(conn) => {
            let info = conn.closed().await;
            assert_eq!(info.code, Some(CloseCode::NOT_PAIRED), "{info:?}");
        }
    }
    server.await.unwrap();
}

#[tokio::test]
async fn unexpected_server_identity_is_refused() {
    let (a, _ta, id_a) = endpoint();
    let (b, trust_b, _id_b) = endpoint();
    trust_b.ids.lock().unwrap().insert(id_a);
    let b_addr = addr(&b);
    tokio::spawn(async move {
        let _ = b.accept().await;
    });
    let impostor_expected = DeviceId([7; 32]);
    let err = a
        .connect(
            PeerAddr::Ip(b_addr),
            ExpectedPeer::Device(impostor_expected),
            ConnectionKind::Bulk,
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(err, TransportError::WrongPeer), "{err:?}");
}

#[tokio::test]
async fn bulk_alpn_is_reported_to_the_acceptor() {
    let (a, ta, id_a) = endpoint();
    let (b, tb, id_b) = endpoint();
    ta.ids.lock().unwrap().insert(id_b);
    tb.ids.lock().unwrap().insert(id_a);
    let b_addr = addr(&b);
    let server = tokio::spawn(async move {
        let conn = b.accept().await.unwrap().unwrap();
        assert_eq!(conn.kind(), ConnectionKind::Bulk);
        let mut rx = conn.accept_uni().await.unwrap();
        let mut buf = vec![0u8; 3];
        let n = rx.read(&mut buf).await.unwrap().unwrap();
        assert!(n > 0);
        b
    });
    let conn = a
        .connect(
            PeerAddr::Ip(b_addr),
            ExpectedPeer::Any,
            ConnectionKind::Bulk,
        )
        .await
        .unwrap();
    let mut tx = conn.open_uni(0).await.unwrap();
    tx.write_all(b"abc").await.unwrap();
    tx.finish().unwrap();
    server.await.unwrap();
}
