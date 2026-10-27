//! macOS: a session `CGEventTap` to observe and swallow, `CGEventPost` to inject,
//! CoreGraphics display list for screens (ADR-0005). Needs the Accessibility permission.
//!
//! Layout coordinates on macOS are points of the global display space (what events use),
//! not backing pixels; `scale_permille` carries the backing scale.

mod capture;
mod inject;

use std::sync::Arc;

use bridge_protocol::input::{LayoutTag, LockState};
use objc2_application_services::{AXIsProcessTrusted, AXIsProcessTrustedWithOptions};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CGPoint};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode,
    CGDisplayScreenSize, CGError, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGGetActiveDisplayList,
};

pub use capture::MacCapture;
pub use inject::MacInjector;

use crate::{LocalScreen, Platform, PlatformError, ScreenInfo};

/// Written to `kCGEventSourceUserData` of every event we post, so our tap skips them.
pub(crate) const INJECTED_MARK: i64 = 0x4252_4447; // "BRDG"

const MAX_DISPLAYS: usize = 32;

pub(crate) fn cursor() -> CGPoint {
    CGEvent::new(None)
        .map(|e| CGEvent::location(Some(&e)))
        .unwrap_or(CGPoint { x: 0.0, y: 0.0 })
}

fn display_ids() -> Vec<CGDirectDisplayID> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: the buffer holds MAX_DISPLAYS entries, `count` is a local out-pointer.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        return Vec::new();
    }
    ids[..(count as usize).min(MAX_DISPLAYS)].to_vec()
}

fn screen(index: usize, id: CGDirectDisplayID) -> LocalScreen {
    let b = CGDisplayBounds(id);
    let w = b.size.width.max(1.0);
    let mode = CGDisplayCopyDisplayMode(id);
    let pixels = CGDisplayMode::pixel_width(mode.as_deref()) as f64;
    let scale = if pixels > 0.0 { pixels / w } else { 1.0 };
    let mm = CGDisplayScreenSize(id);
    LocalScreen {
        id: index as u32,
        px_w: w.round() as u32,
        px_h: b.size.height.max(1.0).round() as u32,
        origin_x: b.origin.x.round() as i32,
        origin_y: b.origin.y.round() as i32,
        scale_permille: (scale * 1000.0).round() as u32,
        size_mm: (mm.width > 1.0 && mm.height > 1.0).then_some((mm.width, mm.height)),
    }
}

#[derive(Default)]
pub struct MacScreens;

impl ScreenInfo for MacScreens {
    fn screens(&self) -> Vec<LocalScreen> {
        let mut ids: Vec<_> = display_ids()
            .into_iter()
            .map(|id| (CGDisplayBounds(id).origin, id))
            .collect();
        ids.sort_by(|a, b| {
            (a.0.x, a.0.y)
                .partial_cmp(&(b.0.x, b.0.y))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ids.iter()
            .enumerate()
            .map(|(i, (_, id))| screen(i, *id))
            .collect()
    }

    fn layout_tag(&self) -> Option<LayoutTag> {
        // TODO(M4c+): Text Input Sources (TISCopyCurrentKeyboardInputSource) for language sync.
        None
    }

    fn locks(&self) -> LockState {
        let flags = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState);
        // Mac keyboards have no Num Lock or Scroll Lock.
        LockState {
            caps: flags.contains(CGEventFlags::MaskAlphaShift),
            num: false,
            scroll: false,
        }
    }
}

/// Asks macOS to show the Accessibility prompt once; returns whether we are trusted.
pub fn request_accessibility() -> bool {
    // SAFETY: the key is the documented CFString constant, the value a CFBoolean.
    unsafe {
        let key: &CFString = objc2_application_services::kAXTrustedCheckOptionPrompt;
        let dict =
            CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
        AXIsProcessTrustedWithOptions(Some(dict.as_ref()))
    }
}

pub fn platform() -> Result<Platform, PlatformError> {
    // SAFETY: no preconditions.
    if !unsafe { AXIsProcessTrusted() } && !request_accessibility() {
        return Err(PlatformError::PermissionRequired(
            "allow Bridge in System Settings → Privacy & Security → Accessibility, then restart it"
                .into(),
        ));
    }
    let screens = Arc::new(MacScreens);
    Ok(Platform {
        capture: Arc::new(MacCapture::new()),
        injector: Arc::new(MacInjector::new(screens.clone())),
        screens,
    })
}
