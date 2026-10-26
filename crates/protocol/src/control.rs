//! Messages of the core (`FeatureId::CORE`) carried on the control stream.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{
    BoundedString, BoundedVec, CloseCode, DecodeError, DeviceId, FeatureDescriptor, FeatureId,
    NegotiatedFeature, PermissionId, Platform, ProtocolVersion, SessionToken, Validate,
    define_messages,
};

pub const MAX_FEATURES: usize = 64;
pub const MAX_PERMISSIONS: usize = 64;
pub const MAX_SCREENS: usize = 64;
pub const MAX_NAME_LEN: usize = 64;

/// Coordinates further than this from the layout origin are rejected (ADR-0006).
pub const MAX_LAYOUT_COORD_MM: f64 = 1_000_000.0;
pub const MAX_SCREEN_SIDE_MM: f64 = 10_000.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Intent {
    Normal,
    Pair,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol: ProtocolVersion,
    pub app_version: BoundedString<32>,
    pub device_id: DeviceId,
    pub name: BoundedString<MAX_NAME_LEN>,
    pub platform: Platform,
    pub features: BoundedVec<FeatureDescriptor, MAX_FEATURES>,
    pub intent: Intent,
    pub resume: Option<SessionToken>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: ProtocolVersion,
    pub app_version: BoundedString<32>,
    pub device_id: DeviceId,
    pub name: BoundedString<MAX_NAME_LEN>,
    pub platform: Platform,
    pub features: BoundedVec<FeatureDescriptor, MAX_FEATURES>,
    pub negotiated: BoundedVec<NegotiatedFeature, MAX_FEATURES>,
    pub session: SessionToken,
}

/// Sent instead of `ServerHello` (or at any time on the control stream) right before
/// the connection is closed with `code`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Reject {
    pub code: CloseCode,
    pub protocol: ProtocolVersion,
    pub message: BoundedString<256>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DeviceState {
    Connected,
    Paused,
    Left,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct StateReason(pub u16);

impl StateReason {
    pub const USER: Self = Self(0);
    pub const NETWORK_CHANGED: Self = Self(1);
    pub const SHUTDOWN: Self = Self(2);
    pub const AUTO_REJOIN: Self = Self(3);
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub state: DeviceState,
    pub reason: StateReason,
}

/// What the sender allows the receiver to do to the sender.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Permissions {
    pub granted: BoundedVec<PermissionId, MAX_PERMISSIONS>,
}

/// Lamport version of the shared layout; ties are broken by the editing device.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct LayoutVersion {
    pub counter: u64,
    pub device: DeviceId,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct LayoutDigest {
    pub version: LayoutVersion,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct LayoutRequest;

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct RectMm {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ScreenPlacement {
    pub device: DeviceId,
    pub screen: u32,
    pub px_width: u32,
    pub px_height: u32,
    /// Position in the device's own virtual desktop, in pixels.
    pub origin_x: i32,
    pub origin_y: i32,
    /// OS scale factor × 1000.
    pub scale_permille: u32,
    /// Placement in the shared physical layout.
    pub rect_mm: RectMm,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct LayoutUpdate {
    pub version: LayoutVersion,
    pub screens: BoundedVec<ScreenPlacement, MAX_SCREENS>,
}

/// Clock sync request; times are monotonic microseconds of the sender (ARCHITECTURE §4.6).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ClockPing {
    pub t0: u64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ClockPong {
    pub t0: u64,
    /// Receive and send times of the responder, in its own clock.
    pub t1: u64,
    pub t2: u64,
}

/// The sender revoked trust; the receiver should forget the pairing.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Unpaired;

/// Which secret the initiator typed or scanned.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PairMethod {
    /// The 6-digit code shown on the responder.
    Code,
    /// The 128-bit secret from the responder's QR code.
    QrSecret,
}

/// SPAKE2 message (ADR-0004). The initiator sends it right after `ClientHello` with
/// `Intent::Pair`, the responder answers with its own.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct PairStart {
    pub method: PairMethod,
    pub spake: BoundedVec<u8, 64>,
}

/// Key confirmation: HMAC over both certificate fingerprints, keyed from the PAKE.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct PairConfirm {
    pub mac: [u8; 32],
}

define_messages! {
    pub enum ControlMessage for FeatureId::CORE {
        1 => ClientHello,
        2 => ServerHello,
        3 => Reject,
        4 => DeviceStatus,
        5 => Permissions,
        6 => LayoutDigest,
        7 => LayoutRequest,
        8 => LayoutUpdate,
        9 => ClockPing,
        10 => ClockPong,
        11 => Unpaired,
        12 => PairStart,
        13 => PairConfirm,
    }
}

fn check_descriptors(features: &[FeatureDescriptor]) -> Result<(), DecodeError> {
    let mut seen = HashSet::with_capacity(features.len());
    for f in features {
        if f.min_version > f.max_version {
            return Err(DecodeError::Invalid("feature version range is empty"));
        }
        if !seen.insert(f.id) {
            return Err(DecodeError::Invalid("duplicate feature id"));
        }
    }
    Ok(())
}

impl Validate for ClientHello {
    fn validate(&self) -> Result<(), DecodeError> {
        check_descriptors(&self.features)
    }
}

impl Validate for ServerHello {
    fn validate(&self) -> Result<(), DecodeError> {
        check_descriptors(&self.features)?;
        if !self.negotiated.windows(2).all(|w| w[0].id < w[1].id) {
            return Err(DecodeError::Invalid(
                "negotiated features must be sorted and unique",
            ));
        }
        Ok(())
    }
}

impl Validate for Reject {}
impl Validate for DeviceStatus {}
impl Validate for LayoutDigest {}
impl Validate for LayoutRequest {}
impl Validate for ClockPing {}
impl Validate for Unpaired {}
impl Validate for PairStart {}
impl Validate for PairConfirm {}

impl Validate for Permissions {
    fn validate(&self) -> Result<(), DecodeError> {
        let unique: HashSet<_> = self.granted.iter().collect();
        if unique.len() != self.granted.len() {
            return Err(DecodeError::Invalid("duplicate permission"));
        }
        Ok(())
    }
}

impl Validate for RectMm {
    fn validate(&self) -> Result<(), DecodeError> {
        let in_range = |v: f64| v.is_finite() && v.abs() <= MAX_LAYOUT_COORD_MM;
        let side = |v: f64| v.is_finite() && v > 0.0 && v <= MAX_SCREEN_SIDE_MM;
        if in_range(self.x) && in_range(self.y) && side(self.w) && side(self.h) {
            Ok(())
        } else {
            Err(DecodeError::Invalid("screen rectangle out of range"))
        }
    }
}

impl Validate for ScreenPlacement {
    fn validate(&self) -> Result<(), DecodeError> {
        let px = 1..=65_535;
        if !px.contains(&self.px_width) || !px.contains(&self.px_height) {
            return Err(DecodeError::Invalid("screen size in pixels out of range"));
        }
        if !(250..=8_000).contains(&self.scale_permille) {
            return Err(DecodeError::Invalid("screen scale out of range"));
        }
        self.rect_mm.validate()
    }
}

impl Validate for LayoutUpdate {
    fn validate(&self) -> Result<(), DecodeError> {
        let mut seen = HashSet::with_capacity(self.screens.len());
        for s in self.screens.iter() {
            s.validate()?;
            if !seen.insert((s.device, s.screen)) {
                return Err(DecodeError::Invalid("duplicate screen"));
            }
        }
        Ok(())
    }
}

impl Validate for ClockPong {
    fn validate(&self) -> Result<(), DecodeError> {
        if self.t1 > self.t2 {
            return Err(DecodeError::Invalid("pong sent before ping was received"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, PROTOCOL_VERSION};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> RectMm {
        RectMm { x, y, w, h }
    }

    fn screen(screen: u32, rect_mm: RectMm) -> ScreenPlacement {
        ScreenPlacement {
            device: DeviceId([1; 32]),
            screen,
            px_width: 1920,
            px_height: 1080,
            origin_x: 0,
            origin_y: 0,
            scale_permille: 1000,
            rect_mm,
        }
    }

    fn roundtrip(m: ControlMessage) -> Result<ControlMessage, DecodeError> {
        let bytes = m.to_envelope().unwrap().to_bytes();
        ControlMessage::from_envelope(&Envelope::parse(&bytes)?)
    }

    #[test]
    fn kinds_are_unique() {
        let set: HashSet<_> = ControlMessage::KINDS.iter().collect();
        assert_eq!(set.len(), ControlMessage::KINDS.len());
    }

    #[test]
    fn unknown_kind_and_foreign_feature_are_reported() {
        assert_eq!(
            ControlMessage::decode(999, &[]),
            Err(DecodeError::UnknownKind {
                feature: FeatureId::CORE,
                kind: 999
            })
        );
        let env = crate::EnvelopeRef {
            feature: FeatureId::FILES,
            kind: 1,
            payload: &[],
        };
        assert!(ControlMessage::from_envelope(&env).is_err());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut env = ControlMessage::from(ClockPing { t0: 5 })
            .to_envelope()
            .unwrap();
        env.payload.push(0);
        assert_eq!(
            ControlMessage::decode(env.kind, &env.payload),
            Err(DecodeError::TrailingBytes(1))
        );
    }

    #[test]
    fn duplicate_features_and_empty_ranges_are_rejected() {
        let fd = |id, min, max| FeatureDescriptor {
            id: FeatureId(id),
            min_version: min,
            max_version: max,
        };
        let mut hello = ClientHello {
            protocol: PROTOCOL_VERSION,
            app_version: BoundedString::truncating("0.1.0"),
            device_id: DeviceId([2; 32]),
            name: BoundedString::truncating("Desktop-PC"),
            platform: Platform::WINDOWS,
            features: BoundedVec::try_from_vec(vec![fd(1, 1, 1), fd(1, 1, 2)]).unwrap(),
            intent: Intent::Normal,
            resume: None,
        };
        assert!(roundtrip(hello.clone().into()).is_err());
        hello.features = BoundedVec::try_from_vec(vec![fd(1, 2, 1)]).unwrap();
        assert!(roundtrip(hello.clone().into()).is_err());
        hello.features = BoundedVec::try_from_vec(vec![fd(1, 1, 2), fd(3, 1, 1)]).unwrap();
        assert_eq!(roundtrip(hello.clone().into()).unwrap(), hello.into());
    }

    #[test]
    fn server_hello_requires_sorted_negotiation() {
        let nf = |id| NegotiatedFeature {
            id: FeatureId(id),
            version: 1,
        };
        let mut hello = ServerHello {
            protocol: PROTOCOL_VERSION,
            app_version: BoundedString::truncating("0.1.0"),
            device_id: DeviceId([3; 32]),
            name: BoundedString::truncating("Laptop"),
            platform: Platform::MACOS,
            features: BoundedVec::new(),
            negotiated: BoundedVec::try_from_vec(vec![nf(2), nf(1)]).unwrap(),
            session: SessionToken([9; 16]),
        };
        assert!(roundtrip(hello.clone().into()).is_err());
        hello.negotiated = BoundedVec::try_from_vec(vec![nf(1), nf(2)]).unwrap();
        assert!(roundtrip(hello.into()).is_ok());
    }

    #[test]
    fn layout_rejects_non_finite_and_out_of_range_values() {
        let ok = LayoutUpdate {
            version: LayoutVersion {
                counter: 1,
                device: DeviceId([1; 32]),
            },
            screens: BoundedVec::try_from_vec(vec![screen(0, rect(0.0, 0.0, 600.0, 340.0))])
                .unwrap(),
        };
        assert!(roundtrip(ok.clone().into()).is_ok());

        for bad in [
            rect(f64::NAN, 0.0, 600.0, 340.0),
            rect(0.0, f64::INFINITY, 600.0, 340.0),
            rect(2e6, 0.0, 600.0, 340.0),
            rect(0.0, 0.0, 0.0, 340.0),
            rect(0.0, 0.0, 600.0, 20_000.0),
        ] {
            let mut m = ok.clone();
            m.screens = BoundedVec::try_from_vec(vec![screen(0, bad)]).unwrap();
            assert!(roundtrip(m.into()).is_err(), "{bad:?}");
        }

        let mut dup = ok.clone();
        let s = screen(0, rect(0.0, 0.0, 600.0, 340.0));
        dup.screens = BoundedVec::try_from_vec(vec![s.clone(), s]).unwrap();
        assert!(roundtrip(dup.into()).is_err());

        let mut tiny = ok.clone();
        let mut s = screen(0, rect(0.0, 0.0, 600.0, 340.0));
        s.px_width = 0;
        tiny.screens = BoundedVec::try_from_vec(vec![s]).unwrap();
        assert!(roundtrip(tiny.into()).is_err());

        let mut scaled = ok;
        let mut s = screen(0, rect(0.0, 0.0, 600.0, 340.0));
        s.scale_permille = 100;
        scaled.screens = BoundedVec::try_from_vec(vec![s]).unwrap();
        assert!(roundtrip(scaled.into()).is_err());
    }

    #[test]
    fn duplicate_permissions_are_rejected() {
        let p = Permissions {
            granted: BoundedVec::try_from_vec(vec![PermissionId::INPUT, PermissionId::INPUT])
                .unwrap(),
        };
        assert!(roundtrip(p.into()).is_err());
    }

    #[test]
    fn pong_times_must_be_ordered() {
        assert!(
            roundtrip(
                ClockPong {
                    t0: 1,
                    t1: 10,
                    t2: 5
                }
                .into()
            )
            .is_err()
        );
        assert!(
            roundtrip(
                ClockPong {
                    t0: 1,
                    t1: 5,
                    t2: 10
                }
                .into()
            )
            .is_ok()
        );
    }

    #[test]
    fn unit_messages_have_empty_payload() {
        let env = ControlMessage::from(Unpaired).to_envelope().unwrap();
        assert!(env.payload.is_empty());
        assert_eq!(
            roundtrip(LayoutRequest.into()).unwrap(),
            LayoutRequest.into()
        );
    }
}
