//! Linux Wayland through xdg-desktop-portal (ADR-0015): InputCapture to take over the
//! pointer at a barrier on the screen edge, RemoteDesktop + EIS to inject. Portals are
//! async D-Bus, so everything runs on one thread with its own runtime; the sync traits
//! talk to it through channels.
//!
//! Not tested against a real compositor yet (LIMITATIONS, MANUAL_TESTS I13).

mod capture;
mod inject;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_protocol::input::{LayoutTag, LockState};
use tokio::sync::mpsc;
use tracing::warn;

pub use capture::WaylandCapture;
pub use inject::WaylandInjector;

use crate::{LocalScreen, Platform, PlatformError, ScreenInfo};

/// True when this session is Wayland and the user did not ask for X11 (XWayland only
/// sees X clients, so the X11 backend would capture almost nothing).
pub fn is_wayland_session() -> bool {
    if std::env::var("BRIDGE_INPUT_BACKEND").is_ok_and(|v| v == "x11") {
        return false;
    }
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(t) => t == "wayland",
        Err(_) => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

/// A zone or output in compositor logical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Rect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// A small push across the edge, as the router expects from a real move.
    pub(crate) fn outward(self) -> (i32, i32) {
        match self {
            Edge::Left => (-2, 0),
            Edge::Right => (2, 0),
            Edge::Top => (0, -2),
            Edge::Bottom => (0, 2),
        }
    }
}

/// A pointer barrier on an outer edge: `(edge, x1, y1, x2, y2)` in the portal's
/// convention (right and bottom barriers sit one pixel past the zone).
pub(crate) type BarrierLine = (Edge, i32, i32, i32, i32);

fn overlaps(a0: i32, a1: i32, b0: i32, b1: i32) -> bool {
    a0 < b1 && b0 < a1
}

/// Barriers on every zone edge that no other zone touches.
pub(crate) fn outer_barriers(zones: &[Rect]) -> Vec<BarrierLine> {
    let mut out = Vec::new();
    for (i, z) in zones.iter().enumerate() {
        let others = || {
            zones
                .iter()
                .enumerate()
                .filter(move |(j, _)| *j != i)
                .map(|(_, o)| o)
        };
        let touches = |edge: Edge| {
            others().any(|o| match edge {
                Edge::Left => o.x + o.w == z.x && overlaps(o.y, o.y + o.h, z.y, z.y + z.h),
                Edge::Right => o.x == z.x + z.w && overlaps(o.y, o.y + o.h, z.y, z.y + z.h),
                Edge::Top => o.y + o.h == z.y && overlaps(o.x, o.x + o.w, z.x, z.x + z.w),
                Edge::Bottom => o.y == z.y + z.h && overlaps(o.x, o.x + o.w, z.x, z.x + z.w),
            })
        };
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            if touches(edge) {
                continue;
            }
            out.push(match edge {
                Edge::Left => (edge, z.x, z.y, z.x, z.y + z.h - 1),
                Edge::Right => (edge, z.x + z.w, z.y, z.x + z.w, z.y + z.h - 1),
                Edge::Top => (edge, z.x, z.y, z.x + z.w - 1, z.y),
                Edge::Bottom => (edge, z.x, z.y + z.h, z.x + z.w - 1, z.y + z.h),
            });
        }
    }
    out
}

pub(crate) fn to_screens(rects: &[Rect], scale: impl Fn(usize) -> f32) -> Vec<LocalScreen> {
    let mut sorted: Vec<(usize, Rect)> = rects.iter().copied().enumerate().collect();
    sorted.sort_by_key(|(_, r)| (r.x, r.y));
    sorted
        .iter()
        .enumerate()
        .map(|(id, (orig, r))| LocalScreen {
            id: id as u32,
            px_w: r.w.max(1) as u32,
            px_h: r.h.max(1) as u32,
            origin_x: r.x,
            origin_y: r.y,
            scale_permille: (scale(*orig).max(0.25) * 1000.0).round() as u32,
            size_mm: None,
        })
        .collect()
}

/// Screens as the portals report them: InputCapture zones, else the EIS regions of
/// the injection device.
#[derive(Default)]
pub struct WaylandScreens {
    pub(crate) zones: Mutex<Vec<LocalScreen>>,
    pub(crate) regions: Mutex<Vec<LocalScreen>>,
}

impl ScreenInfo for WaylandScreens {
    fn screens(&self) -> Vec<LocalScreen> {
        let zones = self.zones.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if zones.is_empty() {
            self.regions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        } else {
            zones
        }
    }

    fn layout_tag(&self) -> Option<LayoutTag> {
        // TODO(M4c+): no portal exposes the active layout; GNOME/KDE-specific D-Bus only.
        None
    }

    fn locks(&self) -> LockState {
        // TODO(M4c+): EIS KeyboardModifiers carries locked mods during capture only.
        LockState::default()
    }
}

pub fn platform() -> Result<Platform, PlatformError> {
    let screens = Arc::new(WaylandScreens::default());
    let (cap_tx, cap_rx) = mpsc::unbounded_channel();
    let (inj_tx, inj_rx) = mpsc::unbounded_channel();
    let capture = Arc::new(WaylandCapture::new(cap_tx));
    let injector = Arc::new(WaylandInjector::new(inj_tx, screens.clone()));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (c, i, s) = (capture.shared(), injector.shared(), screens.clone());
    std::thread::Builder::new()
        .name("bridge-wayland".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            rt.block_on(async move {
                let has_capture = ashpd::desktop::input_capture::InputCapture::new()
                    .await
                    .is_ok();
                let has_remote = ashpd::desktop::remote_desktop::RemoteDesktop::new()
                    .await
                    .is_ok();
                let _ = ready_tx.send(Ok((has_capture, has_remote)));
                let cap = async {
                    if has_capture && let Err(e) = capture::run(c, s, cap_rx).await {
                        warn!("wayland input capture stopped: {e}");
                    }
                };
                let inj = async {
                    if has_remote && let Err(e) = inject::run(i, inj_rx).await {
                        warn!("wayland injection stopped: {e}");
                    }
                };
                tokio::join!(cap, inj);
            });
        })
        .map_err(|e| PlatformError::Os(e.to_string()))?;
    let (has_capture, has_remote) = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| PlatformError::Os("the desktop portal did not answer".into()))?
        .map_err(PlatformError::Os)?;
    match (has_capture, has_remote) {
        (false, false) => {
            return Err(PlatformError::Unsupported(
                "this Wayland compositor offers neither the InputCapture nor the RemoteDesktop \
                 portal (GNOME 46+ and KDE Plasma 6.1+ do); a hotkey-only mode arrives with the \
                 desktop app, or log in to an X11 session"
                    .into(),
            ));
        }
        (false, true) => warn!(
            "no InputCapture portal: this device can be controlled but cannot share its own mouse"
        ),
        (true, false) => {
            warn!("no RemoteDesktop portal: this device can share its mouse but not be controlled")
        }
        (true, true) => {}
    }
    Ok(Platform {
        capture,
        injector,
        screens,
    })
}

/// `$XDG_STATE_HOME/bridge`, where the RemoteDesktop restore token lives.
pub(crate) fn state_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/state")))
        .map(|d| d.join("bridge"))
}

/// Microseconds of `CLOCK_MONOTONIC`, the EI timestamp base.
pub(crate) fn ei_now() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000 + t.tv_nsec as u64 / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn barriers_skip_shared_edges() {
        let one = outer_barriers(&[r(0, 0, 1920, 1080)]);
        assert_eq!(one.len(), 4);
        assert!(one.contains(&(Edge::Right, 1920, 0, 1920, 1079)));
        assert!(one.contains(&(Edge::Top, 0, 0, 1919, 0)));
        assert!(one.contains(&(Edge::Bottom, 0, 1080, 1919, 1080)));

        let two = outer_barriers(&[r(0, 0, 1920, 1080), r(1920, 0, 1280, 1024)]);
        assert_eq!(two.len(), 6, "{two:?}");
        assert!(!two.iter().any(|b| b.0 == Edge::Right && b.1 == 1920));
        assert!(!two.iter().any(|b| b.0 == Edge::Left && b.1 == 1920));
        assert!(two.contains(&(Edge::Right, 3200, 0, 3200, 1023)));

        // Stacked vertically without horizontal overlap: no shared edge.
        let apart = outer_barriers(&[r(0, 0, 100, 100), r(100, 100, 100, 100)]);
        assert_eq!(apart.len(), 8);
    }

    #[test]
    fn screens_are_sorted_and_scaled() {
        let s = to_screens(&[r(1920, 0, 1280, 1024), r(0, 0, 1920, 1080)], |i| {
            if i == 1 { 2.0 } else { 1.0 }
        });
        assert_eq!(s[0].origin_x, 0);
        assert_eq!(s[0].scale_permille, 2000);
        assert_eq!(s[1].id, 1);
        assert_eq!(s[1].px_w, 1280);
        assert_eq!(Edge::Left.outward(), (-2, 0));
    }
}
