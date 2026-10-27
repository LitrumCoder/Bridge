//! Shared cursor and keyboard (ARCHITECTURE §8.1): glues the sans-IO router and
//! injection state to the network and the OS.
//!
//! The router runs inside the OS hook (`CaptureSink`), so each event is decided without
//! waiting; what it queues for peers is drained here, on the node's thread, after a
//! wake-up posted by the hook.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use bridge_core::input::{
    CaptureEvent, Decision, Hotkeys, InjectAction, Injected, InputRouter, Layout, Remap,
    RouterAction, RouterEvent,
};
use bridge_core::{Feature, FeatureCx, FeatureError, FeatureInfo, Micros, PeerDownReason, PeerId};
use bridge_platform::{CaptureSink, Platform, PlatformError};
use bridge_protocol::control::{DeviceState, LayoutUpdate};
use bridge_protocol::input::{InputMessage, Motion};
use bridge_protocol::{
    ChannelClass, DeviceId, FeatureId, PermissionId, decode_payload, encode_payload,
};
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub struct InputConfig {
    /// Ctrl ↔ Cmd when a non-Mac controls this Mac.
    pub swap_ctrl_cmd: bool,
    pub hotkeys: Hotkeys,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            swap_ctrl_cmd: true,
            hotkeys: Hotkeys::default(),
        }
    }
}

/// Posted by the capture thread when the router has something for the network.
pub struct Wake;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Receives router events (entered a device, returned, …) on the node thread.
pub type RouterListener = Arc<dyn Fn(&RouterEvent) + Send + Sync>;

/// Lets other features steer the router without owning it.
#[derive(Clone)]
pub struct InputControl(Arc<Shared>);

impl InputControl {
    /// A file drag is in progress: crossing with the button held hands it over.
    pub fn set_drag_active(&self, active: bool) {
        self.0.router().set_drag_active(active);
    }
}

struct Shared {
    router: Mutex<InputRouter>,
    epoch: Instant,
    wake_pending: AtomicBool,
    wake: Waker,
}

impl Shared {
    fn now(&self) -> Micros {
        Micros::from_duration(self.epoch.elapsed())
    }

    fn router(&self) -> MutexGuard<'_, InputRouter> {
        self.router.lock().unwrap_or_else(|p| p.into_inner())
    }
}

struct Sink(Arc<Shared>);

impl CaptureSink for Sink {
    fn on_event(&self, ev: CaptureEvent) -> Decision {
        let now = self.0.now();
        let (decision, wake) = {
            let mut r = self.0.router();
            let d = r.on_event(now, ev);
            (d, r.has_actions())
        };
        if wake && !self.0.wake_pending.swap(true, Ordering::AcqRel) {
            (self.0.wake)();
        }
        decision
    }
}

pub struct InputFeature {
    shared: Arc<Shared>,
    injected: Injected,
    platform: Platform,
    my_platform: bridge_protocol::Platform,
    config: InputConfig,
    last_event: Option<RouterEvent>,
    listener: Option<RouterListener>,
}

impl InputFeature {
    /// Starts capturing local input. `wake` must make the node call `on_local` with
    /// [`Wake`] soon (the daemon passes its local channel).
    pub fn new(
        me: DeviceId,
        platform: Platform,
        config: InputConfig,
        wake: Waker,
    ) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared {
            router: Mutex::new(InputRouter::new(
                me,
                Layout::default(),
                config.hotkeys.clone(),
            )),
            epoch: Instant::now(),
            wake_pending: AtomicBool::new(false),
            wake,
        });
        platform.capture.start(Arc::new(Sink(shared.clone())))?;
        Ok(Self {
            shared,
            injected: Injected::default(),
            platform,
            my_platform: bridge_protocol::Platform::current(),
            config,
            last_event: None,
            listener: None,
        })
    }

    pub fn control(&self) -> InputControl {
        InputControl(self.shared.clone())
    }

    pub fn on_router_event(mut self, listener: RouterListener) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Last router event (entered a device, returned, locked, shake), for the UI.
    pub fn last_event(&self) -> Option<&RouterEvent> {
        self.last_event.as_ref()
    }

    fn remap_for(&self, peer: Option<bridge_protocol::Platform>) -> Remap {
        let mac = bridge_protocol::Platform::MACOS;
        Remap {
            swap_ctrl_gui: self.config.swap_ctrl_cmd
                && self.my_platform == mac
                && peer.is_some_and(|p| p != mac),
        }
    }

    fn refresh_availability(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId) {
        let available = cx
            .peer(&peer)
            .is_some_and(|v| v.allows_us && v.state == DeviceState::Connected);
        self.shared.router().set_available(peer, available);
    }

    fn apply(&self, actions: Vec<InjectAction>) {
        for a in actions {
            let inj = &self.platform.injector;
            let r = match &a {
                InjectAction::MoveAbs { screen, x, y } => inj.move_abs(*screen, *x, *y),
                InjectAction::MoveRel { dx, dy } => inj.move_rel(*dx, *dy),
                InjectAction::Key { usage, down } => inj.key(*usage, *down),
                InjectAction::Button { button, down } => inj.button(*button, *down),
                InjectAction::Wheel { dx, dy } => inj.wheel(*dx, *dy),
                InjectAction::Text(t) => inj.text(t),
                InjectAction::SetLayout(l) => inj.set_layout(l),
                InjectAction::SetLocks(l) => inj.set_locks(*l),
            };
            if let Err(e) = r {
                warn!("injection failed: {e}");
            }
        }
    }

    /// Sends what the router queued and applies its local effects.
    fn drain(&mut self, cx: &mut FeatureCx<'_>) {
        self.shared.wake_pending.store(false, Ordering::Release);
        let now = self.shared.now();
        let actions: Vec<RouterAction> = {
            let mut r = self.shared.router();
            r.set_locks(self.platform.screens.locks());
            r.set_layout_tag(self.platform.screens.layout_tag());
            std::iter::from_fn(|| r.poll_action(now)).collect()
        };
        for action in actions {
            match action {
                RouterAction::Send { peer, msg } => match msg.to_envelope() {
                    Ok(env) => cx.send_encoded(peer, env.kind, env.payload),
                    Err(e) => warn!("cannot encode input message: {e}"),
                },
                RouterAction::Motion { peer, motion } => match encode_payload(&motion) {
                    Ok(bytes) => cx.send_datagram(peer, bytes),
                    Err(e) => warn!("cannot encode motion: {e}"),
                },
                RouterAction::Grab(g) => self.platform.capture.set_grab(g),
                RouterAction::Warp { x, y } => self.platform.capture.warp(x, y),
                RouterAction::Event(e) => {
                    debug!(?e, "input router");
                    if let Some(l) = &self.listener {
                        l(&e);
                    }
                    // TODO(M9): edge glow and shake locator overlays, drawn by the UI process.
                    self.last_event = Some(e);
                }
            }
        }
    }
}

impl Feature for InputFeature {
    fn info(&self) -> FeatureInfo {
        FeatureInfo {
            id: FeatureId::INPUT,
            name: "input",
            versions: 1..=1,
            channel: ChannelClass::InputEvents,
            permission: Some(PermissionId::INPUT),
        }
    }

    fn on_peer_up(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, _version: u16) {
        self.refresh_availability(cx, peer);
        self.drain(cx);
    }

    fn on_peer_update(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId) {
        self.refresh_availability(cx, peer);
        if cx
            .peer(&peer)
            .is_some_and(|v| v.state != DeviceState::Connected)
        {
            // A paused peer stops controlling us too.
            let release = self.injected.source_gone(&peer);
            self.apply(release);
        }
        self.drain(cx);
    }

    fn on_peer_down(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, _reason: PeerDownReason) {
        self.shared.router().set_available(peer, false);
        let release = self.injected.source_gone(&peer);
        self.apply(release);
        self.drain(cx);
    }

    fn on_message(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        let msg = InputMessage::decode(kind, payload)?;
        let remap = self.remap_for(cx.peer(&peer).map(|v| v.platform));
        let actions = self.injected.on_message(peer, &msg, remap);
        self.apply(actions);
        Ok(())
    }

    fn on_datagram(
        &mut self,
        _cx: &mut FeatureCx<'_>,
        peer: PeerId,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        let motion: Motion = decode_payload(payload)?;
        if let Some(a) = self.injected.on_motion(peer, motion) {
            self.apply(vec![a]);
        }
        Ok(())
    }

    fn on_layout(&mut self, cx: &mut FeatureCx<'_>, layout: &LayoutUpdate) {
        self.shared.router().set_layout(Layout::from_update(layout));
        self.drain(cx);
    }

    fn on_local(&mut self, cx: &mut FeatureCx<'_>, event: Box<dyn std::any::Any + Send>) {
        if event.is::<Wake>() {
            self.drain(cx);
        }
    }

    fn on_tick(&mut self, cx: &mut FeatureCx<'_>) {
        self.drain(cx);
    }
}

impl Drop for InputFeature {
    fn drop(&mut self) {
        self.platform.capture.stop();
    }
}
