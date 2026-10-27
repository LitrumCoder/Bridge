//! RemoteDesktop + ConnectToEIS: we are an EI sender. The permission dialog is shown
//! once; the restore token keeps it from coming back where the portal persists it.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};
use futures_util::StreamExt;
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{Rect, WaylandScreens, ei_now, state_dir, to_screens};
use crate::keymap::hid_to_evdev;
use crate::{InputInjector, PlatformError, ScreenInfo};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    MoveAbs(f32, f32),
    MoveRel(f32, f32),
    Key(u32, bool),
    Button(u32, bool),
    Wheel(i32, i32),
    Text(String),
}

pub(crate) struct Shared {
    ready: AtomicBool,
    screens: Arc<WaylandScreens>,
}

pub struct WaylandInjector {
    tx: mpsc::UnboundedSender<Op>,
    shared: Arc<Shared>,
}

impl WaylandInjector {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Op>, screens: Arc<WaylandScreens>) -> Self {
        Self {
            tx,
            shared: Arc::new(Shared {
                ready: AtomicBool::new(false),
                screens,
            }),
        }
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    fn send(&self, op: Op) -> Result<(), PlatformError> {
        if !self.shared.ready.load(Ordering::Acquire) {
            return Err(PlatformError::PermissionRequired(
                "waiting for the remote desktop permission (portal dialog)".into(),
            ));
        }
        self.tx
            .send(op)
            .map_err(|_| PlatformError::Os("the Wayland injection task stopped".into()))
    }
}

pub(crate) fn evdev_button(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x113,
        MouseButton::Forward => 0x114,
    }
}

impl InputInjector for WaylandInjector {
    fn move_abs(&self, screen: u32, x: i32, y: i32) -> Result<(), PlatformError> {
        let screens = self.shared.screens.screens();
        let s = screens
            .iter()
            .find(|s| s.id == screen)
            .ok_or_else(|| PlatformError::Os(format!("no screen {screen}")))?;
        let gx = s.origin_x + x.clamp(0, s.px_w as i32 - 1);
        let gy = s.origin_y + y.clamp(0, s.px_h as i32 - 1);
        self.send(Op::MoveAbs(gx as f32, gy as f32))
    }

    fn move_rel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        self.send(Op::MoveRel(dx as f32, dy as f32))
    }

    fn key(&self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        let code = hid_to_evdev(usage).ok_or_else(|| {
            PlatformError::Unsupported(format!("key {:#04x} has no evdev code", usage.0))
        })?;
        self.send(Op::Key(u32::from(code), down))
    }

    fn button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        self.send(Op::Button(evdev_button(button), down))
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        // EI discrete scroll is in 1/120 notches too, but positive means down.
        self.send(Op::Wheel(dx, -dy))
    }

    fn text(&self, text: &str) -> Result<(), PlatformError> {
        self.send(Op::Text(text.to_owned()))
    }

    fn set_layout(&self, tag: &LayoutTag) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(format!(
            "Wayland portals cannot switch the layout (asked for {})",
            tag.id
        )))
    }

    fn set_locks(&self, _locks: LockState) -> Result<(), PlatformError> {
        // TODO(M4c+): lock state is unknown on Wayland (see WaylandScreens::locks).
        Ok(())
    }
}

#[derive(Default)]
struct Devices {
    list: Vec<(Device, bool)>,
    sequence: u32,
}

impl Devices {
    fn with(&self, cap: DeviceCapability) -> Option<&Device> {
        self.list
            .iter()
            .find(|(d, resumed)| *resumed && d.has_capability(cap))
            .map(|(d, _)| d)
    }

    fn regions(&self) -> Vec<(Rect, f32)> {
        self.list
            .iter()
            .filter(|(d, _)| d.has_capability(DeviceCapability::PointerAbsolute))
            .flat_map(|(d, _)| d.regions().iter())
            .map(|r| {
                (
                    Rect {
                        x: r.x as i32,
                        y: r.y as i32,
                        w: r.width as i32,
                        h: r.height as i32,
                    },
                    r.scale,
                )
            })
            .collect()
    }
}

fn find<T: ei::Interface>(
    devices: &Devices,
    cap: DeviceCapability,
) -> Result<(&Device, T), PlatformError> {
    devices
        .with(cap)
        .and_then(|d| d.interface::<T>().map(|i| (d, i)))
        .ok_or_else(|| PlatformError::Unsupported(format!("no EIS device with {cap:?}")))
}

fn apply(devices: &Devices, serial: u32, op: Op) -> Result<(), PlatformError> {
    use DeviceCapability as C;
    let device = match op {
        Op::MoveRel(dx, dy) => {
            let (d, p) = find::<ei::Pointer>(devices, C::Pointer)?;
            p.motion_relative(dx, dy);
            d
        }
        Op::MoveAbs(x, y) => {
            let (d, p) = find::<ei::PointerAbsolute>(devices, C::PointerAbsolute)?;
            p.motion_absolute(x, y);
            d
        }
        Op::Button(code, down) => {
            let (d, b) = find::<ei::Button>(devices, C::Button)?;
            b.button(
                code,
                if down {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                },
            );
            d
        }
        Op::Wheel(dx, dy) => {
            let (d, s) = find::<ei::Scroll>(devices, C::Scroll)?;
            s.scroll_discrete(dx, dy);
            d
        }
        Op::Key(code, down) => {
            let (d, k) = find::<ei::Keyboard>(devices, C::Keyboard)?;
            k.key(
                code,
                if down {
                    ei::keyboard::KeyState::Press
                } else {
                    ei::keyboard::KeyState::Released
                },
            );
            d
        }
        Op::Text(text) => {
            let (d, t) = find::<ei::Text>(devices, C::Text)?;
            t.utf8(&text);
            d
        }
    };
    device.device().frame(serial, ei_now());
    Ok(())
}

fn token_path() -> Option<std::path::PathBuf> {
    state_dir().map(|d| d.join("remote-desktop.token"))
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    mut rx: mpsc::UnboundedReceiver<Op>,
) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    let portal = RemoteDesktop::new().await.map_err(|e| s(&e))?;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| s(&e))?;
    let token = token_path().and_then(|p| std::fs::read_to_string(p).ok());
    portal
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref().map(str::trim)),
        )
        .await
        .map_err(|e| s(&e))?
        .response()
        .map_err(|e| s(&e))?;
    let started = portal
        .start(&session, None, StartOptions::default())
        .await
        .map_err(|e| s(&e))?
        .response()
        .map_err(|e| format!("remote desktop permission: {e}"))?;
    if let (Some(t), Some(path)) = (started.restore_token(), token_path()) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, t) {
            warn!("cannot save the portal restore token: {e}");
        }
    }
    info!(devices = ?started.devices(), "remote desktop session started");
    let fd = portal
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .map_err(|e| s(&e))?;
    let context = ei::Context::new(UnixStream::from(fd)).map_err(|e| s(&e))?;
    let (conn, mut events) = context
        .handshake_tokio("bridge", ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| s(&e))?;
    let mut devices = Devices::default();
    let publish = |devices: &Devices| {
        let regions = devices.regions();
        *shared
            .screens
            .regions
            .lock()
            .unwrap_or_else(|p| p.into_inner()) =
            to_screens(&regions.iter().map(|r| r.0).collect::<Vec<_>>(), |i| {
                regions[i].1
            });
    };
    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(EiEvent::SeatAdded(seat))) => {
                    seat.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Scroll
                            | DeviceCapability::Button
                            | DeviceCapability::Text,
                    );
                    let _ = context.flush();
                }
                Some(Ok(EiEvent::DeviceAdded(d))) => {
                    devices.list.push((d.device, false));
                    publish(&devices);
                }
                Some(Ok(EiEvent::DeviceRemoved(d))) => {
                    devices.list.retain(|(x, _)| x != &d.device);
                    publish(&devices);
                }
                Some(Ok(EiEvent::DeviceResumed(d))) => {
                    devices.sequence = devices.sequence.wrapping_add(1);
                    d.device.device().start_emulating(d.serial, devices.sequence);
                    if let Some(entry) = devices.list.iter_mut().find(|(x, _)| x == &d.device) {
                        entry.1 = true;
                    }
                    let _ = context.flush();
                    shared.ready.store(true, Ordering::Release);
                }
                Some(Ok(EiEvent::DevicePaused(d))) => {
                    if let Some(entry) = devices.list.iter_mut().find(|(x, _)| x == &d.device) {
                        entry.1 = false;
                    }
                }
                Some(Ok(EiEvent::Disconnected(_))) | None => {
                    shared.ready.store(false, Ordering::Release);
                    return Err("EIS disconnected".into());
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(s(&e)),
            },
            op = rx.recv() => {
                let Some(op) = op else { return Ok(()) };
                if let Err(e) = apply(&devices, conn.serial(), op) {
                    warn!("wayland injection: {e}");
                }
                let _ = context.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(MouseButton::Left), 0x110);
        assert_eq!(evdev_button(MouseButton::Forward), 0x114);
        assert!(
            super::super::capture::button(evdev_button(MouseButton::Back))
                == Some(MouseButton::Back)
        );
    }
}
