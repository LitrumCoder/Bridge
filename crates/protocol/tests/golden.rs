//! Golden wire bytes (ADR-0003): a published message must keep its exact encoding.
//! After an intentional, versioned change run with `BRIDGE_BLESS=1` to rewrite the file.

use std::fmt::Write as _;
use std::path::PathBuf;

use bridge_protocol::control::*;
use bridge_protocol::*;

fn samples() -> Vec<(&'static str, ControlMessage)> {
    let a = DeviceId([0x11; 32]);
    let b = DeviceId([0x22; 32]);
    let features = BoundedVec::try_from_vec(vec![
        FeatureDescriptor {
            id: FeatureId::INPUT,
            min_version: 1,
            max_version: 1,
        },
        FeatureDescriptor {
            id: FeatureId::MIDI,
            min_version: 1,
            max_version: 2,
        },
    ])
    .expect("fits");
    vec![
        (
            "client_hello",
            ClientHello {
                protocol: ProtocolVersion { major: 1, minor: 0 },
                app_version: BoundedString::truncating("0.1.0"),
                device_id: a,
                name: BoundedString::truncating("Desktop-PC"),
                platform: Platform::WINDOWS,
                features: features.clone(),
                intent: Intent::Normal,
                resume: None,
            }
            .into(),
        ),
        (
            "server_hello",
            ServerHello {
                protocol: ProtocolVersion { major: 1, minor: 0 },
                app_version: BoundedString::truncating("0.1.0"),
                device_id: b,
                name: BoundedString::truncating("Ноутбук"),
                platform: Platform::MACOS,
                features,
                negotiated: BoundedVec::try_from_vec(vec![NegotiatedFeature {
                    id: FeatureId::INPUT,
                    version: 1,
                }])
                .expect("fits"),
                session: SessionToken([0x33; 16]),
            }
            .into(),
        ),
        (
            "reject",
            Reject {
                code: CloseCode::INCOMPATIBLE_VERSION,
                protocol: ProtocolVersion { major: 2, minor: 1 },
                message: BoundedString::truncating("update required"),
            }
            .into(),
        ),
        (
            "device_status",
            DeviceStatus {
                state: DeviceState::Left,
                reason: StateReason::NETWORK_CHANGED,
            }
            .into(),
        ),
        (
            "permissions",
            Permissions {
                granted: BoundedVec::try_from_vec(vec![PermissionId::INPUT, PermissionId::MIDI])
                    .expect("fits"),
            }
            .into(),
        ),
        (
            "layout_digest",
            LayoutDigest {
                version: LayoutVersion {
                    counter: 7,
                    device: a,
                },
            }
            .into(),
        ),
        ("layout_request", LayoutRequest.into()),
        (
            "layout_update",
            LayoutUpdate {
                version: LayoutVersion {
                    counter: 7,
                    device: a,
                },
                screens: BoundedVec::try_from_vec(vec![ScreenPlacement {
                    device: a,
                    screen: 0,
                    px_width: 2560,
                    px_height: 1440,
                    origin_x: -2560,
                    origin_y: 0,
                    scale_permille: 1250,
                    rect_mm: RectMm {
                        x: -597.0,
                        y: 0.0,
                        w: 597.0,
                        h: 336.0,
                    },
                }])
                .expect("fits"),
            }
            .into(),
        ),
        ("clock_ping", ClockPing { t0: 1_000_000 }.into()),
        (
            "clock_pong",
            ClockPong {
                t0: 1_000_000,
                t1: 2_000_000,
                t2: 2_000_042,
            }
            .into(),
        ),
        ("unpaired", Unpaired.into()),
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[test]
fn control_messages_match_golden_bytes() {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "golden", "control.hex"]
        .iter()
        .collect();
    let rendered: String = samples()
        .iter()
        .map(|(name, msg)| {
            format!(
                "{name} {}\n",
                hex(&msg.to_envelope().expect("encode").to_bytes())
            )
        })
        .collect();

    if std::env::var_os("BRIDGE_BLESS").is_some() {
        std::fs::write(&path, &rendered).expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .expect("golden file missing; run with BRIDGE_BLESS=1 once")
        .replace("\r\n", "\n");
    assert_eq!(
        rendered, expected,
        "wire format changed; see ADR-0003 before blessing"
    );
}

#[test]
fn every_control_kind_has_a_golden_sample() {
    let mut kinds: Vec<u16> = samples().iter().map(|(_, m)| m.kind()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ControlMessage::KINDS.to_vec());
}

#[test]
fn golden_samples_decode_back() {
    for (name, msg) in samples() {
        let bytes = msg.to_envelope().expect("encode").to_bytes();
        let env = Envelope::parse(&bytes).expect(name);
        assert_eq!(
            ControlMessage::from_envelope(&env).expect(name),
            msg,
            "{name}"
        );
    }
}
