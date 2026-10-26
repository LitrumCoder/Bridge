//! What a device shows when it opens its pairing window, and the QR payload format:
//! `bridge://pair?fp=<device id>&addr=<ip:port>&s=<secret>` (base32, ADR-0004).

use std::net::SocketAddr;
use std::time::Duration;

use bridge_core::PairingSecret;
use bridge_protocol::{DeviceId, base32_decode, base32_encode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingInvite {
    pub code: String,
    pub qr_uri: String,
    pub expires_in: Duration,
}

impl PairingInvite {
    pub(crate) fn new(
        id: DeviceId,
        addr: SocketAddr,
        code: &PairingSecret,
        qr: &PairingSecret,
        expires_in: Duration,
    ) -> Self {
        let secret = qr.qr_bytes().map(|b| base32_encode(&b)).unwrap_or_default();
        Self {
            code: code.code().unwrap_or_default().to_owned(),
            qr_uri: format!("bridge://pair?fp={}&addr={addr}&s={secret}", id.to_base32()),
            expires_in,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrPayload {
    pub device: DeviceId,
    pub addrs: Vec<SocketAddr>,
    pub secret: PairingSecret,
}

/// Parses a scanned or pasted pairing URI; `None` for anything malformed.
pub fn parse_pairing_uri(uri: &str) -> Option<QrPayload> {
    let query = uri.trim().strip_prefix("bridge://pair?")?;
    let (mut device, mut secret, mut addrs) = (None, None, Vec::new());
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        match k {
            "fp" => device = DeviceId::from_base32(v),
            "addr" => {
                for a in v.split(',').filter(|a| !a.is_empty()) {
                    addrs.push(a.parse().ok()?);
                }
            }
            "s" => {
                let bytes: [u8; 16] = base32_decode(v)?.try_into().ok()?;
                secret = Some(PairingSecret::from_qr(bytes));
            }
            _ => {}
        }
    }
    Some(QrPayload {
        device: device?,
        addrs,
        secret: secret?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_uri_roundtrips() {
        let id = DeviceId([9; 32]);
        let addr: SocketAddr = "192.168.1.20:47800".parse().expect("addr");
        let code = PairingSecret::from_code("482913").expect("code");
        let qr = PairingSecret::from_qr([0xAB; 16]);
        let invite = PairingInvite::new(id, addr, &code, &qr, Duration::from_secs(120));
        assert_eq!(invite.code, "482913");
        let parsed = parse_pairing_uri(&invite.qr_uri).expect("parse");
        assert_eq!(parsed.device, id);
        assert_eq!(parsed.addrs, vec![addr]);
        assert_eq!(parsed.secret, qr);
    }

    #[test]
    fn malformed_uris_are_rejected() {
        for bad in [
            "",
            "https://example.com",
            "bridge://pair?fp=abc&s=abc",
            "bridge://pair?addr=1.2.3.4:5",
            "bridge://pair?fp=aaaa&addr=nonsense&s=aaaa",
            "bridge://pair?novalue",
        ] {
            assert!(parse_pairing_uri(bad).is_none(), "{bad}");
        }
    }
}
