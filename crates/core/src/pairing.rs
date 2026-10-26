//! Pairing by a 6-digit code or a QR secret (ADR-0004): SPAKE2 bound to both devices'
//! certificate identities, then HMAC key confirmation in both directions.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use bridge_protocol::DeviceId;
use bridge_protocol::control::PairMethod;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::Micros;

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const MAX_FAILURES: usize = 3;
const LOCKOUT: Duration = Duration::from_secs(300);
const CONFIRM_LABEL: &[u8] = b"bridge-pair-v1";

/// Source of cryptographic randomness supplied by the driver (the core never asks the OS).
pub trait Entropy: Send {
    fn fill(&mut self, buf: &mut [u8]);
}

struct Rng<'a>(&'a mut dyn Entropy);

impl rand_core::RngCore for Rng<'_> {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.0.fill(&mut b);
        u32::from_le_bytes(b)
    }

    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.0.fill(&mut b);
        u64::from_le_bytes(b)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.0.fill(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for Rng<'_> {}

/// The secret both users share: the code typed in, or the bytes from the QR code.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingSecret {
    method: PairMethod,
    bytes: Vec<u8>,
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PairingSecret({:?}, <redacted>)", self.method)
    }
}

impl PairingSecret {
    /// Accepts exactly six ASCII digits; spaces and dashes are ignored ("123 456").
    pub fn from_code(code: &str) -> Option<Self> {
        let digits: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
        (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then(|| Self {
            method: PairMethod::Code,
            bytes: digits.into_bytes(),
        })
    }

    pub fn from_qr(secret: [u8; 16]) -> Self {
        Self {
            method: PairMethod::QrSecret,
            bytes: secret.to_vec(),
        }
    }

    /// A fresh 6-digit code with uniform distribution (rejection sampling).
    pub fn generate_code(entropy: &mut dyn Entropy) -> Self {
        loop {
            let mut b = [0u8; 4];
            entropy.fill(&mut b);
            let n = u32::from_le_bytes(b);
            // 4_294_000_000 is the largest multiple of 1_000_000 below 2^32.
            if n < 4_294_000_000 {
                let code = format!("{:06}", n % 1_000_000);
                return Self {
                    method: PairMethod::Code,
                    bytes: code.into_bytes(),
                };
            }
        }
    }

    pub fn generate_qr(entropy: &mut dyn Entropy) -> Self {
        let mut s = [0u8; 16];
        entropy.fill(&mut s);
        Self::from_qr(s)
    }

    pub fn method(&self) -> PairMethod {
        self.method
    }

    /// The code as shown to the user (only for `PairMethod::Code`).
    pub fn code(&self) -> Option<&str> {
        (self.method == PairMethod::Code)
            .then(|| std::str::from_utf8(&self.bytes).ok())
            .flatten()
    }

    /// The raw QR secret (only for `PairMethod::QrSecret`).
    pub fn qr_bytes(&self) -> Option<[u8; 16]> {
        (self.method == PairMethod::QrSecret)
            .then(|| self.bytes.as_slice().try_into().ok())
            .flatten()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Side {
    Initiator,
    Responder,
}

pub(crate) struct Pake {
    state: Spake2<Ed25519Group>,
    initiator: DeviceId,
    responder: DeviceId,
}

/// Starts SPAKE2. Identities are the certificate-proven ids, so a relay in the middle
/// (which must present its own certificate to each side) gets different keys.
pub(crate) fn start(
    side: Side,
    secret: &PairingSecret,
    initiator: DeviceId,
    responder: DeviceId,
    entropy: &mut dyn Entropy,
) -> (Pake, Vec<u8>) {
    let password = Password::new(&secret.bytes);
    let (a, b) = (Identity::new(&initiator.0), Identity::new(&responder.0));
    let rng = Rng(entropy);
    let (state, msg) = match side {
        Side::Initiator => Spake2::<Ed25519Group>::start_a_with_rng(&password, &a, &b, rng),
        Side::Responder => Spake2::<Ed25519Group>::start_b_with_rng(&password, &a, &b, rng),
    };
    (
        Pake {
            state,
            initiator,
            responder,
        },
        msg,
    )
}

impl Pake {
    pub(crate) fn finish(self, their_msg: &[u8]) -> Option<PairingKey> {
        let key = self.state.finish(their_msg).ok()?;
        Some(PairingKey {
            key,
            initiator: self.initiator,
            responder: self.responder,
        })
    }
}

pub(crate) struct PairingKey {
    key: Vec<u8>,
    initiator: DeviceId,
    responder: DeviceId,
}

impl PairingKey {
    fn hmac(&self, side: Side) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key)
            .expect("HMAC accepts keys of any length");
        mac.update(CONFIRM_LABEL);
        mac.update(match side {
            Side::Initiator => b"I",
            Side::Responder => b"R",
        });
        mac.update(&self.initiator.0);
        mac.update(&self.responder.0);
        mac
    }

    pub(crate) fn confirmation(&self, side: Side) -> [u8; 32] {
        self.hmac(side).finalize().into_bytes().into()
    }

    /// Constant-time check of the other side's confirmation.
    pub(crate) fn verify(&self, side: Side, mac: &[u8; 32]) -> bool {
        self.hmac(side).verify_slice(mac).is_ok()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PairingError {
    #[error("pairing is locked after repeated failures")]
    Locked { until: Micros },
}

/// The responder's pairing window: open on request, single use, locked after
/// repeated failures.
#[derive(Default)]
pub(crate) struct PairingWindow {
    code: Option<PairingSecret>,
    qr: Option<PairingSecret>,
    expires: Micros,
    failures: VecDeque<Micros>,
    locked_until: Option<Micros>,
}

impl PairingWindow {
    pub(crate) fn open(
        &mut self,
        now: Micros,
        code: PairingSecret,
        qr: PairingSecret,
        duration: Duration,
    ) -> Result<(), PairingError> {
        if let Some(until) = self.locked_until.filter(|u| now < *u) {
            return Err(PairingError::Locked { until });
        }
        self.code = Some(code);
        self.qr = Some(qr);
        self.expires = now + duration;
        Ok(())
    }

    pub(crate) fn close(&mut self) {
        self.code = None;
        self.qr = None;
    }

    pub(crate) fn is_open(&self, now: Micros) -> bool {
        (self.code.is_some() || self.qr.is_some()) && now < self.expires
    }

    pub(crate) fn expires(&self) -> Option<Micros> {
        (self.code.is_some() || self.qr.is_some()).then_some(self.expires)
    }

    pub(crate) fn secret(&self, now: Micros, method: PairMethod) -> Option<PairingSecret> {
        if !self.is_open(now) {
            return None;
        }
        match method {
            PairMethod::Code => self.code.clone(),
            PairMethod::QrSecret => self.qr.clone(),
        }
    }

    /// One attempt per code: any failure closes the window; three within the lockout
    /// period lock pairing for `LOCKOUT`.
    pub(crate) fn record_failure(&mut self, now: Micros) {
        self.close();
        self.failures.push_back(now);
        while self
            .failures
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > LOCKOUT)
        {
            self.failures.pop_front();
        }
        if self.failures.len() >= MAX_FAILURES {
            self.locked_until = Some(now + LOCKOUT);
            self.failures.clear();
        }
    }

    pub(crate) fn record_success(&mut self) {
        self.close();
        self.failures.clear();
    }

    pub(crate) fn locked_until(&self, now: Micros) -> Option<Micros> {
        self.locked_until.filter(|u| now < *u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) struct Counter(u64);

    impl Entropy for Counter {
        fn fill(&mut self, buf: &mut [u8]) {
            for b in buf {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *b = (self.0 >> 56) as u8;
            }
        }
    }

    fn ids() -> (DeviceId, DeviceId) {
        (DeviceId([1; 32]), DeviceId([2; 32]))
    }

    fn run(
        secret_a: &PairingSecret,
        secret_b: &PairingSecret,
        ids_a: (DeviceId, DeviceId),
        ids_b: (DeviceId, DeviceId),
    ) -> bool {
        let mut e = Counter(7);
        let (pa, ma) = start(Side::Initiator, secret_a, ids_a.0, ids_a.1, &mut e);
        let (pb, mb) = start(Side::Responder, secret_b, ids_b.0, ids_b.1, &mut e);
        let (Some(ka), Some(kb)) = (pa.finish(&mb), pb.finish(&ma)) else {
            return false;
        };
        let ok_b = kb.verify(Side::Initiator, &ka.confirmation(Side::Initiator));
        let ok_a = ka.verify(Side::Responder, &kb.confirmation(Side::Responder));
        assert_eq!(ok_a, ok_b, "both sides must agree");
        ok_a
    }

    #[test]
    fn same_code_and_identities_confirm() {
        let s = PairingSecret::from_code("123 456").unwrap();
        assert!(run(&s, &s, ids(), ids()));
        let q = PairingSecret::from_qr([9; 16]);
        assert!(run(&q, &q, ids(), ids()));
    }

    #[test]
    fn wrong_code_fails() {
        let a = PairingSecret::from_code("123456").unwrap();
        let b = PairingSecret::from_code("123457").unwrap();
        assert!(!run(&a, &b, ids(), ids()));
    }

    #[test]
    fn relay_with_different_certificates_fails_even_with_the_right_code() {
        // The initiator talks to a relay presenting certificate M; the responder sees
        // the relay as certificate M2. SPAKE messages are forwarded unchanged.
        let s = PairingSecret::from_code("000000").unwrap();
        let (a, b) = ids();
        let (m, m2) = (DeviceId([7; 32]), DeviceId([8; 32]));
        assert!(!run(&s, &s, (a, m), (m2, b)));
    }

    #[test]
    fn codes_parse_and_generate() {
        assert!(PairingSecret::from_code("12345").is_none());
        assert!(PairingSecret::from_code("12a456").is_none());
        assert_eq!(
            PairingSecret::from_code("12-34-56").unwrap().code(),
            Some("123456")
        );
        let mut e = Counter(1);
        for _ in 0..100 {
            let c = PairingSecret::generate_code(&mut e);
            let code = c.code().unwrap();
            assert_eq!(code.len(), 6);
            assert!(PairingSecret::from_code(code).is_some());
        }
        let q = PairingSecret::generate_qr(&mut e);
        assert!(q.qr_bytes().is_some() && q.code().is_none());
        assert!(
            !format!("{q:?}").contains("9"),
            "secret must not leak in Debug"
        );
    }

    #[test]
    fn window_single_use_expiry_and_lockout() {
        let mut w = PairingWindow::default();
        let mut e = Counter(3);
        let t0 = Micros(1_000_000);
        let fresh = |e: &mut Counter| {
            (
                PairingSecret::generate_code(e),
                PairingSecret::generate_qr(e),
            )
        };
        assert!(!w.is_open(t0));
        let (c, q) = fresh(&mut e);
        w.open(t0, c, q, PAIRING_WINDOW).unwrap();
        assert!(w.secret(t0, PairMethod::Code).is_some());
        assert!(
            w.secret(t0 + PAIRING_WINDOW, PairMethod::Code).is_none(),
            "expired"
        );
        w.record_failure(t0);
        assert!(!w.is_open(t0), "one attempt per code");
        for i in 1..3 {
            let (c, q) = fresh(&mut e);
            w.open(t0, c, q, PAIRING_WINDOW).unwrap();
            w.record_failure(t0 + Duration::from_secs(i));
        }
        let (c, q) = fresh(&mut e);
        assert!(matches!(
            w.open(t0 + Duration::from_secs(10), c, q, PAIRING_WINDOW),
            Err(PairingError::Locked { .. })
        ));
        assert!(w.locked_until(t0 + Duration::from_secs(10)).is_some());
        let later = t0 + LOCKOUT + Duration::from_secs(10);
        let (c, q) = fresh(&mut e);
        assert!(w.open(later, c, q, PAIRING_WINDOW).is_ok());
        w.record_success();
        assert!(!w.is_open(later));
        assert_eq!(w.expires(), None);
    }
}
