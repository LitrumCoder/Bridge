//! Router behaviour and the "nothing stays pressed" invariant (ADR-0005).

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::time::Duration;

use bridge_core::Micros;
use bridge_core::input::*;
use bridge_protocol::DeviceId;
use bridge_protocol::control::RectMm;
use bridge_protocol::input::{HidUsage, InputMessage, MouseButton};
use proptest::prelude::*;

const ME: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
const C: DeviceId = DeviceId([3; 32]);

fn scr(device: DeviceId, x_mm: f64) -> Screen {
    Screen {
        device,
        id: 0,
        px_w: 1920,
        px_h: 1080,
        origin_x: 0,
        origin_y: 0,
        rect: RectMm {
            x: x_mm,
            y: 0.0,
            w: 500.0,
            h: 280.0,
        },
    }
}

/// ME | B | C side by side, all 1920x1080 at the same density.
fn router() -> InputRouter {
    let layout = Layout::new(vec![scr(ME, 0.0), scr(B, 500.0), scr(C, 1000.0)]);
    let mut r = InputRouter::new(ME, layout, Hotkeys::default());
    r.set_available(B, true);
    r.set_available(C, true);
    r
}

fn drain(r: &mut InputRouter, now: Micros) -> Vec<RouterAction> {
    std::iter::from_fn(|| r.poll_action(now)).collect()
}

fn key(usage: HidUsage, down: bool) -> CaptureEvent {
    CaptureEvent::Key {
        usage,
        down,
        repeat: false,
    }
}

fn sent_to(actions: &[RouterAction], peer: DeviceId) -> Vec<InputMessage> {
    actions
        .iter()
        .filter_map(|a| match a {
            RouterAction::Send { peer: p, msg } if *p == peer => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

/// Moves the local cursor from near the right edge across into B.
fn cross_to_b(r: &mut InputRouter, t: Micros) -> Vec<RouterAction> {
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 1918,
                y: 500,
                dx: 5,
                dy: 0
            }
        ),
        Decision::Swallow
    );
    drain(r, t)
}

#[test]
fn crossing_grabs_input_and_sends_enter_then_motion() {
    let mut r = router();
    let t = Micros(1);
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 100,
                y: 100,
                dx: 5,
                dy: 0
            }
        ),
        Decision::Pass
    );
    let acts = cross_to_b(&mut r, t);
    assert!(matches!(acts[0], RouterAction::Grab(true)));
    assert!(matches!(sent_to(&acts, B)[0], InputMessage::Enter(ref e) if e.x < 10 && e.y == 500));
    assert!(
        acts.iter()
            .any(|a| matches!(a, RouterAction::Motion { peer, .. } if *peer == B))
    );
    assert_eq!(r.current_peer(), Some(B));

    assert_eq!(r.on_event(t, key(HidUsage::A, true)), Decision::Swallow);
    assert_eq!(
        r.on_event(t, CaptureEvent::Wheel { dx: 0, dy: 120 }),
        Decision::Swallow
    );
    let acts = drain(&mut r, t);
    assert_eq!(sent_to(&acts, B).len(), 2);
}

#[test]
fn moving_back_releases_on_the_peer_and_warps_home() {
    let mut r = router();
    let t = Micros(1);
    cross_to_b(&mut r, t);
    r.on_event(t, key(HidUsage::A, true));
    drain(&mut r, t);
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 0,
                y: 0,
                dx: -20,
                dy: 0
            }
        ),
        Decision::Swallow
    );
    let acts = drain(&mut r, t);
    let msgs = sent_to(&acts, B);
    assert!(matches!(msgs[0], InputMessage::ReleaseAll(_)));
    assert!(matches!(msgs[1], InputMessage::Leave(_)));
    assert!(acts.iter().any(|a| matches!(a, RouterAction::Grab(false))));
    assert!(
        acts.iter()
            .any(|a| matches!(a, RouterAction::Warp { x, .. } if *x > 1890))
    );
    assert_eq!(r.current_peer(), None);
    // The key held across the edge was released on B already; its release goes nowhere.
    assert_eq!(r.on_event(t, key(HidUsage::A, false)), Decision::Swallow);
    assert!(drain(&mut r, t).is_empty());
}

#[test]
fn keys_pressed_locally_are_released_locally_even_from_remote() {
    let mut r = router();
    let t = Micros(1);
    assert_eq!(
        r.on_event(t, key(HidUsage::LEFT_SHIFT, true)),
        Decision::Pass
    );
    cross_to_b(&mut r, t);
    assert_eq!(
        r.on_event(t, key(HidUsage::LEFT_SHIFT, false)),
        Decision::Pass,
        "the local OS saw the press"
    );
    assert!(sent_to(&drain(&mut r, t), B).is_empty());
}

#[test]
fn held_button_blocks_crossing_unless_dragging_files() {
    let mut r = router();
    let t = Micros(1);
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Button {
                button: MouseButton::Left,
                down: true
            }
        ),
        Decision::Pass
    );
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 1918,
                y: 500,
                dx: 5,
                dy: 0
            }
        ),
        Decision::Pass
    );
    assert_eq!(r.current_peer(), None);
    r.set_drag_active(true);
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 1918,
                y: 500,
                dx: 5,
                dy: 0
            }
        ),
        Decision::Swallow
    );
    assert_eq!(r.current_peer(), Some(B));
}

#[test]
fn a_file_drag_hands_the_button_over_and_drops_on_both_sides() {
    let mut r = router();
    let t = Micros(1);
    let left = |down| CaptureEvent::Button {
        button: MouseButton::Left,
        down,
    };
    assert_eq!(r.on_event(t, left(true)), Decision::Pass);
    r.set_drag_active(true);
    let acts = cross_to_b(&mut r, t);
    let msgs = sent_to(&acts, B);
    assert!(
        msgs.iter().any(
            |m| matches!(m, InputMessage::Button(b) if b.down && b.button == MouseButton::Left)
        ),
        "B gets the button down so its OS can run the drag: {msgs:?}"
    );
    // Pushing back towards A does not leave B before the drop.
    for _ in 0..10 {
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 0,
                y: 0,
                dx: -500,
                dy: 0,
            },
        );
    }
    assert_eq!(r.current_peer(), Some(B));
    drain(&mut r, t);
    assert_eq!(
        r.on_event(t, left(false)),
        Decision::Pass,
        "the local drag loop ends too (in Bridge's own window)"
    );
    let msgs = sent_to(&drain(&mut r, t), B);
    assert!(
        msgs.iter()
            .any(|m| matches!(m, InputMessage::Button(b) if !b.down)),
        "and B drops: {msgs:?}"
    );
    // After the drop the cursor moves freely again.
    r.set_drag_active(false);
    r.on_event(
        t,
        CaptureEvent::Motion {
            x: 0,
            y: 0,
            dx: -3000,
            dy: 0,
        },
    );
    assert_eq!(r.current_peer(), None);
}

#[test]
fn peer_disappearing_returns_control_home() {
    let mut r = router();
    let t = Micros(1);
    cross_to_b(&mut r, t);
    r.on_event(t, key(HidUsage::A, true));
    drain(&mut r, t);
    r.set_available(B, false);
    let acts = drain(&mut r, t);
    assert!(acts.iter().any(|a| matches!(a, RouterAction::Grab(false))));
    assert!(acts.iter().any(|a| matches!(
        a,
        RouterAction::Event(RouterEvent::Returned {
            reason: ReturnReason::PeerUnavailable
        })
    )));
    assert!(
        sent_to(&acts, B).is_empty(),
        "nothing is sent to a gone peer"
    );
    assert_eq!(r.on_event(t, key(HidUsage::A, false)), Decision::Swallow);
    // B is gone: the edge is now a wall towards it, but C beyond B is reachable.
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 1918,
                y: 500,
                dx: 5,
                dy: 0
            }
        ),
        Decision::Swallow
    );
    assert_eq!(
        r.current_peer(),
        Some(C),
        "layout collapses over the absent device"
    );
}

#[test]
fn moving_on_to_a_third_device_switches_targets() {
    let mut r = router();
    let t = Micros(1);
    cross_to_b(&mut r, t);
    r.on_event(
        t,
        CaptureEvent::Motion {
            x: 0,
            y: 0,
            dx: 2000,
            dy: 0,
        },
    );
    let acts = drain(&mut r, t);
    let to_b = sent_to(&acts, B);
    assert!(matches!(
        to_b[..],
        [InputMessage::ReleaseAll(_), InputMessage::Leave(_)]
    ));
    assert!(matches!(sent_to(&acts, C)[0], InputMessage::Enter(_)));
    assert_eq!(r.current_peer(), Some(C));
}

#[test]
fn hotkeys_lock_go_to_device_and_emergency() {
    let mut r = router();
    let t = Micros(1);
    let chord = |r: &mut InputRouter, k| {
        r.on_event(t, key(HidUsage::LEFT_CTRL, true));
        r.on_event(t, key(HidUsage::LEFT_ALT, true));
        let d = r.on_event(t, key(k, true));
        r.on_event(t, key(k, false));
        r.on_event(t, key(HidUsage::LEFT_ALT, false));
        r.on_event(t, key(HidUsage::LEFT_CTRL, false));
        d
    };
    assert_eq!(chord(&mut r, HidUsage::L), Decision::Swallow);
    assert!(r.is_locked());
    assert_eq!(
        r.on_event(
            t,
            CaptureEvent::Motion {
                x: 1918,
                y: 500,
                dx: 5,
                dy: 0
            }
        ),
        Decision::Pass,
        "locked"
    );
    chord(&mut r, HidUsage::L);
    assert!(!r.is_locked());

    chord(&mut r, HidUsage(HidUsage::F1.0 + 2));
    assert_eq!(
        r.current_peer(),
        Some(C),
        "F3 = third device in layout order"
    );
    drain(&mut r, t);
    r.on_event(t, key(HidUsage::LEFT_CTRL, true));
    r.on_event(t, key(HidUsage::LEFT_ALT, true));
    r.on_event(t, key(HidUsage::LEFT_SHIFT, true));
    r.on_event(t, key(HidUsage::ESCAPE, true));
    assert_eq!(r.current_peer(), None, "emergency return");
    let acts = drain(&mut r, t);
    assert!(acts.iter().any(|a| matches!(
        a,
        RouterAction::Event(RouterEvent::Returned {
            reason: ReturnReason::Emergency
        })
    )));
}

#[test]
fn watchdog_returns_control_when_the_network_task_stalls() {
    let mut r = router();
    let t = Micros(1_000_000);
    cross_to_b(&mut r, t);
    r.on_event(t, key(HidUsage::A, true));
    let later = t + WATCHDOG + Duration::from_millis(10);
    r.on_event(
        later,
        CaptureEvent::Motion {
            x: 0,
            y: 0,
            dx: 1,
            dy: 0,
        },
    );
    assert_eq!(r.current_peer(), None);
    let acts = drain(&mut r, later);
    assert!(acts.iter().any(|a| matches!(
        a,
        RouterAction::Event(RouterEvent::Returned {
            reason: ReturnReason::Watchdog
        })
    )));
}

#[test]
fn shaking_on_a_remote_screen_raises_an_event() {
    let mut r = router();
    let mut t = 1_000_000u64;
    cross_to_b(&mut r, Micros(t));
    // Stay inside B (its middle) while shaking.
    r.on_event(
        Micros(t),
        CaptureEvent::Motion {
            x: 0,
            y: 0,
            dx: 900,
            dy: 0,
        },
    );
    let mut seen = false;
    for i in 0..12 {
        for _ in 0..4 {
            t += 10_000;
            r.on_event(
                Micros(t),
                CaptureEvent::Motion {
                    x: 0,
                    y: 0,
                    dx: if i % 2 == 0 { 15 } else { -15 },
                    dy: 0,
                },
            );
        }
        seen |= drain(&mut r, Micros(t)).iter().any(
            |a| matches!(a, RouterAction::Event(RouterEvent::Shake { device }) if *device == B),
        );
    }
    assert!(seen);
}

#[derive(Clone, Debug)]
enum Op {
    Down(u16),
    Up(usize),
    Button(bool),
    Move(i32),
    PeerDown(u8),
    PeerUp(u8),
    Repeat(usize),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        3 => (0x04u16..0x0A).prop_map(Op::Down),
        1 => prop_oneof![Just(Op::Down(0xE0)), Just(Op::Down(0xE2)), Just(Op::Down(0x0F)), Just(Op::Down(0x3A)), Just(Op::Down(0x3C))],
        3 => any::<usize>().prop_map(Op::Up),
        1 => any::<bool>().prop_map(Op::Button),
        3 => prop_oneof![Just(-3000), Just(-25), Just(25), Just(3000), -2500i32..2500].prop_map(Op::Move),
        1 => (0u8..2).prop_map(Op::PeerDown),
        1 => (0u8..2).prop_map(Op::PeerUp),
        1 => any::<usize>().prop_map(Op::Repeat),
    ];
    proptest::collection::vec(op, 1..120)
}

/// One source, two receivers, a lossless link that drops everything for a peer that is
/// down. After any sequence and releasing every physical key, no OS holds anything.
fn run_model(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let mut r = router();
    let peers = [B, C];
    let mut up = [true, true];
    let mut receivers = [Injected::default(), Injected::default()];
    let mut remote_os: [HashSet<HidUsage>; 2] = Default::default();
    let mut remote_buttons = [0i32; 2];
    let mut local_os: HashSet<HidUsage> = HashSet::new();
    let mut local_button = false;
    let mut physical: Vec<HidUsage> = Vec::new();
    let mut physical_button = false;
    let mut cursor = (960i32, 500i32);
    let mut t = 1_000u64;

    let deliver = |r: &mut InputRouter,
                   t: Micros,
                   up: &[bool; 2],
                   receivers: &mut [Injected; 2],
                   remote_os: &mut [HashSet<HidUsage>; 2],
                   remote_buttons: &mut [i32; 2],
                   cursor: &mut (i32, i32)| {
        while let Some(a) = r.poll_action(t) {
            match a {
                RouterAction::Send { peer, msg } => {
                    let i = peers.iter().position(|p| *p == peer).unwrap();
                    if !up[i] {
                        continue;
                    }
                    for act in receivers[i].on_message(ME, &msg, Remap::default()) {
                        match act {
                            InjectAction::Key { usage, down: true } => {
                                remote_os[i].insert(usage);
                            }
                            InjectAction::Key { usage, down: false } => {
                                remote_os[i].remove(&usage);
                            }
                            InjectAction::Button { down, .. } => {
                                remote_buttons[i] += if down { 1 } else { -1 }
                            }
                            _ => {}
                        }
                    }
                }
                RouterAction::Warp { x, y } => *cursor = (x, y),
                _ => {}
            }
        }
    };

    for op in ops {
        t += 1_000;
        let now = Micros(t);
        match op {
            Op::Down(u) => {
                let k = HidUsage(u);
                if physical.contains(&k) {
                    continue;
                }
                physical.push(k);
                if r.on_event(
                    now,
                    CaptureEvent::Key {
                        usage: k,
                        down: true,
                        repeat: false,
                    },
                ) == Decision::Pass
                {
                    local_os.insert(k);
                }
            }
            Op::Repeat(i) if !physical.is_empty() => {
                let k = physical[i % physical.len()];
                if r.on_event(
                    now,
                    CaptureEvent::Key {
                        usage: k,
                        down: true,
                        repeat: true,
                    },
                ) == Decision::Pass
                {
                    local_os.insert(k);
                }
            }
            Op::Repeat(_) => {}
            Op::Up(i) if !physical.is_empty() => {
                let k = physical.remove(i % physical.len());
                if r.on_event(
                    now,
                    CaptureEvent::Key {
                        usage: k,
                        down: false,
                        repeat: false,
                    },
                ) == Decision::Pass
                {
                    local_os.remove(&k);
                }
            }
            Op::Up(_) => {}
            Op::Button(down) => {
                if down == physical_button {
                    continue;
                }
                physical_button = down;
                if r.on_event(
                    now,
                    CaptureEvent::Button {
                        button: MouseButton::Left,
                        down,
                    },
                ) == Decision::Pass
                {
                    local_button = down;
                }
            }
            Op::Move(dx) => {
                if r.on_event(
                    now,
                    CaptureEvent::Motion {
                        x: cursor.0,
                        y: cursor.1,
                        dx,
                        dy: 0,
                    },
                ) == Decision::Pass
                {
                    cursor.0 = (cursor.0 + dx).clamp(0, 1919);
                }
            }
            Op::PeerDown(i) => {
                let i = usize::from(i);
                if up[i] {
                    up[i] = false;
                    r.set_available(peers[i], false);
                    for act in receivers[i].source_gone(&ME) {
                        if let InjectAction::Key { usage, down: false } = act {
                            remote_os[i].remove(&usage);
                        }
                        if let InjectAction::Button { down: false, .. } = act {
                            remote_buttons[i] -= 1;
                        }
                    }
                }
            }
            Op::PeerUp(i) => {
                let i = usize::from(i);
                if !up[i] {
                    up[i] = true;
                    r.set_available(peers[i], true);
                }
            }
        }
        deliver(
            &mut r,
            now,
            &up,
            &mut receivers,
            &mut remote_os,
            &mut remote_buttons,
            &mut cursor,
        );
    }

    // The user lets go of everything.
    t += 1_000;
    for k in physical.drain(..) {
        if r.on_event(
            Micros(t),
            CaptureEvent::Key {
                usage: k,
                down: false,
                repeat: false,
            },
        ) == Decision::Pass
        {
            local_os.remove(&k);
        }
    }
    if physical_button
        && r.on_event(
            Micros(t),
            CaptureEvent::Button {
                button: MouseButton::Left,
                down: false,
            },
        ) == Decision::Pass
    {
        local_button = false;
    }
    deliver(
        &mut r,
        Micros(t),
        &up,
        &mut receivers,
        &mut remote_os,
        &mut remote_buttons,
        &mut cursor,
    );

    prop_assert!(local_os.is_empty(), "local OS still holds {:?}", local_os);
    prop_assert!(!local_button, "local button stuck");
    for i in 0..2 {
        prop_assert!(
            remote_os[i].is_empty(),
            "peer {} still holds {:?}",
            i,
            remote_os[i]
        );
        prop_assert_eq!(remote_buttons[i], 0, "peer {} button balance", i);
        prop_assert_eq!(receivers[i].pressed_count(), 0);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn no_key_or_button_stays_pressed_anywhere(ops in ops()) {
        run_model(ops)?;
    }
}
