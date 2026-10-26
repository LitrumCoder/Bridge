//! Pure logic of the shared cursor and keyboard (ARCHITECTURE §4.3–4.5, ADR-0005/0006).

pub mod geometry;
mod inject;
mod keys;
mod router;
mod shake;

pub use geometry::{CursorPos, Layout, Screen, ScreenRef};
pub use inject::{InjectAction, Injected, Remap};
pub use keys::{Chord, HotkeyAction, Hotkeys, Mods};
pub use router::{
    CaptureEvent, Decision, InputRouter, ReturnReason, RouterAction, RouterEvent, WATCHDOG,
};
