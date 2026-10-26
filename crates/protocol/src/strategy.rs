//! proptest strategies for protocol types, shared with other crates' tests and fuzz
//! harnesses (feature `proptest`).

use std::collections::BTreeMap;

use proptest::collection::{btree_map, btree_set};
use proptest::prelude::*;

use crate::control::*;
use crate::{
    BoundedString, BoundedVec, CloseCode, DeviceId, FeatureDescriptor, FeatureId,
    NegotiatedFeature, PermissionId, Platform, ProtocolVersion, SessionToken,
};

pub fn device_id() -> impl Strategy<Value = DeviceId> {
    any::<[u8; 32]>().prop_map(DeviceId)
}

pub fn bounded_string<const N: usize>() -> impl Strategy<Value = BoundedString<N>> {
    ".{0,40}".prop_map(|s: String| BoundedString::truncating(&s))
}

pub fn protocol_version() -> impl Strategy<Value = ProtocolVersion> {
    (any::<u16>(), any::<u16>()).prop_map(|(major, minor)| ProtocolVersion { major, minor })
}

pub fn feature_descriptors() -> impl Strategy<Value = BoundedVec<FeatureDescriptor, MAX_FEATURES>> {
    btree_map(any::<u16>(), (any::<u16>(), any::<u16>()), 0..8).prop_map(|m| {
        let v = m
            .into_iter()
            .map(|(id, (a, b))| FeatureDescriptor {
                id: FeatureId(id),
                min_version: a.min(b),
                max_version: a.max(b),
            })
            .collect();
        BoundedVec::try_from_vec(v).unwrap_or_default()
    })
}

fn negotiated() -> impl Strategy<Value = BoundedVec<NegotiatedFeature, MAX_FEATURES>> {
    btree_map(any::<u16>(), any::<u16>(), 0..8).prop_map(|m| {
        let v = m
            .into_iter()
            .map(|(id, version)| NegotiatedFeature {
                id: FeatureId(id),
                version,
            })
            .collect();
        BoundedVec::try_from_vec(v).unwrap_or_default()
    })
}

pub fn rect_mm() -> impl Strategy<Value = RectMm> {
    let coord = -MAX_LAYOUT_COORD_MM..=MAX_LAYOUT_COORD_MM;
    let side = 0.01..=MAX_SCREEN_SIDE_MM;
    (coord.clone(), coord, side.clone(), side).prop_map(|(x, y, w, h)| RectMm { x, y, w, h })
}

pub fn screen_placement() -> impl Strategy<Value = ScreenPlacement> {
    (
        device_id(),
        any::<u32>(),
        (1u32..=65_535, 1u32..=65_535),
        (any::<i32>(), any::<i32>()),
        250u32..=8_000,
        rect_mm(),
    )
        .prop_map(
            |(device, screen, (w, h), (ox, oy), scale, rect_mm)| ScreenPlacement {
                device,
                screen,
                px_width: w,
                px_height: h,
                origin_x: ox,
                origin_y: oy,
                scale_permille: scale,
                rect_mm,
            },
        )
}

pub fn layout_version() -> impl Strategy<Value = LayoutVersion> {
    (any::<u64>(), device_id()).prop_map(|(counter, device)| LayoutVersion { counter, device })
}

pub fn layout_update() -> impl Strategy<Value = LayoutUpdate> {
    (
        layout_version(),
        proptest::collection::vec(screen_placement(), 0..6),
    )
        .prop_map(|(version, screens)| {
            let unique: BTreeMap<_, _> = screens
                .into_iter()
                .map(|s| ((s.device, s.screen), s))
                .collect();
            LayoutUpdate {
                version,
                screens: BoundedVec::try_from_vec(unique.into_values().collect())
                    .unwrap_or_default(),
            }
        })
}

fn client_hello() -> impl Strategy<Value = ClientHello> {
    (
        protocol_version(),
        bounded_string::<32>(),
        device_id(),
        bounded_string::<MAX_NAME_LEN>(),
        any::<u8>().prop_map(Platform),
        feature_descriptors(),
        prop_oneof![Just(Intent::Normal), Just(Intent::Pair)],
        proptest::option::of(any::<[u8; 16]>().prop_map(SessionToken)),
    )
        .prop_map(
            |(protocol, app_version, device_id, name, platform, features, intent, resume)| {
                ClientHello {
                    protocol,
                    app_version,
                    device_id,
                    name,
                    platform,
                    features,
                    intent,
                    resume,
                }
            },
        )
}

fn server_hello() -> impl Strategy<Value = ServerHello> {
    (
        protocol_version(),
        bounded_string::<32>(),
        device_id(),
        bounded_string::<MAX_NAME_LEN>(),
        any::<u8>().prop_map(Platform),
        feature_descriptors(),
        negotiated(),
        any::<[u8; 16]>().prop_map(SessionToken),
    )
        .prop_map(
            |(protocol, app_version, device_id, name, platform, features, negotiated, session)| {
                ServerHello {
                    protocol,
                    app_version,
                    device_id,
                    name,
                    platform,
                    features,
                    negotiated,
                    session,
                }
            },
        )
}

fn device_state() -> impl Strategy<Value = DeviceState> {
    prop_oneof![
        Just(DeviceState::Connected),
        Just(DeviceState::Paused),
        Just(DeviceState::Left)
    ]
}

pub fn control_message() -> impl Strategy<Value = ControlMessage> {
    prop_oneof![
        client_hello().prop_map(ControlMessage::from),
        server_hello().prop_map(ControlMessage::from),
        (any::<u32>(), protocol_version(), bounded_string::<256>()).prop_map(|(c, p, m)| {
            Reject {
                code: CloseCode(c),
                protocol: p,
                message: m,
            }
            .into()
        }),
        (device_state(), any::<u16>()).prop_map(|(state, r)| DeviceStatus {
            state,
            reason: StateReason(r)
        }
        .into()),
        btree_set(any::<u16>(), 0..8).prop_map(|s| {
            let v = s.into_iter().map(PermissionId).collect();
            Permissions {
                granted: BoundedVec::try_from_vec(v).unwrap_or_default(),
            }
            .into()
        }),
        layout_version().prop_map(|version| LayoutDigest { version }.into()),
        Just(LayoutRequest.into()),
        layout_update().prop_map(ControlMessage::from),
        any::<u64>().prop_map(|t0| ClockPing { t0 }.into()),
        (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(t0, a, b)| ClockPong {
            t0,
            t1: a.min(b),
            t2: a.max(b)
        }
        .into()),
        Just(Unpaired.into()),
        (
            any::<bool>(),
            proptest::collection::vec(any::<u8>(), 0..=64)
        )
            .prop_map(|(qr, v)| {
                PairStart {
                    method: if qr {
                        PairMethod::QrSecret
                    } else {
                        PairMethod::Code
                    },
                    spake: BoundedVec::try_from_vec(v).unwrap_or_default(),
                }
                .into()
            }),
        any::<[u8; 32]>().prop_map(|mac| PairConfirm { mac }.into()),
    ]
}

fn layout_tag() -> impl Strategy<Value = crate::input::LayoutTag> {
    (bounded_string::<16>(), bounded_string::<64>())
        .prop_map(|(lang, id)| crate::input::LayoutTag { lang, id })
}

pub fn input_message() -> impl Strategy<Value = crate::input::InputMessage> {
    use crate::input::*;
    let coord = -MAX_COORD..=MAX_COORD;
    let button = prop_oneof![
        Just(MouseButton::Left),
        Just(MouseButton::Right),
        Just(MouseButton::Middle),
        Just(MouseButton::Back),
        Just(MouseButton::Forward),
    ];
    let layout = layout_tag();
    prop_oneof![
        (
            any::<u32>(),
            coord.clone(),
            coord,
            any::<(bool, bool, bool)>(),
            proptest::option::of(layout_tag())
        )
            .prop_map(|(screen, x, y, (caps, num, scroll), layout)| {
                Enter {
                    screen,
                    x,
                    y,
                    locks: LockState { caps, num, scroll },
                    layout,
                }
                .into()
            }),
        Just(Leave.into()),
        (0u16..=HidUsage::MAX, any::<bool>(), any::<bool>()).prop_map(|(u, down, rep)| Key {
            usage: HidUsage(u),
            down,
            repeat: rep && down
        }
        .into()),
        (button, any::<bool>()).prop_map(|(button, down)| Button { button, down }.into()),
        (-MAX_WHEEL..=MAX_WHEEL, -MAX_WHEEL..=MAX_WHEEL)
            .prop_map(|(dx, dy)| Wheel { dx, dy }.into()),
        Just(ReleaseAll.into()),
        layout.prop_map(|layout| SetLayout { layout }.into()),
        "[a-zA-Zа-яА-Я0-9 ]{0,40}".prop_map(|s: String| Text {
            text: BoundedString::truncating(&s)
        }
        .into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, FrameDecoder, encode_frame};

    proptest! {
        #[test]
        fn input_message_roundtrips(msg in input_message()) {
            let env = msg.to_envelope().unwrap();
            let back = crate::input::InputMessage::from_envelope(&Envelope::parse(&env.to_bytes()).unwrap()).unwrap();
            prop_assert_eq!(back, msg);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn control_message_roundtrips_through_envelope_and_frame(msg in control_message()) {
            let env = msg.to_envelope().unwrap();
            let mut wire = Vec::new();
            let limit = crate::ChannelClass::Control.max_frame_len();
            encode_frame(&env.to_bytes(), limit, &mut wire).unwrap();

            let mut dec = FrameDecoder::new(limit);
            dec.push(&wire);
            let frame = dec.next_frame().unwrap().unwrap();
            let decoded = ControlMessage::from_envelope(&Envelope::parse(&frame).unwrap()).unwrap();
            prop_assert_eq!(decoded, msg);
        }

        #[test]
        fn encoding_is_deterministic(msg in control_message()) {
            let a = msg.to_envelope().unwrap().to_bytes();
            let b = msg.clone().to_envelope().unwrap().to_bytes();
            prop_assert_eq!(a, b);
        }
    }
}
