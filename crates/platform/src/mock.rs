//! Scriptable implementations for tests (feature `mock`): the test drives capture and
//! inspects what was injected.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};
use tokio::sync::Notify;

use crate::{
    CaptureEvent, CaptureSink, ClipChange, ClipFormat, Clipboard, ClipboardProvider, ClipboardSink,
    Decision, DragSink, DragSource, DropItem, DropProgress, DropSink, DropTarget, InputCapture,
    InputInjector, LocalScreen, Platform, PlatformError, ScreenInfo,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Default)]
pub struct MockCapture {
    sink: Mutex<Option<Arc<dyn CaptureSink>>>,
    grabbed: AtomicBool,
    warps: Mutex<Vec<(i32, i32)>>,
}

impl MockCapture {
    /// Delivers an event as the OS hook would; returns the router's decision.
    pub fn emit(&self, ev: CaptureEvent) -> Decision {
        let sink = lock(&self.sink).clone();
        sink.map_or(Decision::Pass, |s| s.on_event(ev))
    }

    pub fn is_grabbed(&self) -> bool {
        self.grabbed.load(Ordering::SeqCst)
    }

    pub fn warps(&self) -> Vec<(i32, i32)> {
        lock(&self.warps).clone()
    }

    pub fn is_started(&self) -> bool {
        lock(&self.sink).is_some()
    }
}

impl InputCapture for MockCapture {
    fn start(&self, sink: Arc<dyn CaptureSink>) -> Result<(), PlatformError> {
        *lock(&self.sink) = Some(sink);
        Ok(())
    }

    fn set_grab(&self, grab: bool) {
        self.grabbed.store(grab, Ordering::SeqCst);
    }

    fn warp(&self, x: i32, y: i32) {
        lock(&self.warps).push((x, y));
    }

    fn stop(&self) {
        *lock(&self.sink) = None;
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Injected {
    MoveAbs { screen: u32, x: i32, y: i32 },
    MoveRel { dx: i32, dy: i32 },
    Key { usage: HidUsage, down: bool },
    Button { button: MouseButton, down: bool },
    Wheel { dx: i32, dy: i32 },
    Text(String),
    Layout(LayoutTag),
    Locks(LockState),
}

#[derive(Default)]
pub struct MockInjector {
    log: Mutex<Vec<Injected>>,
    changed: Notify,
}

impl MockInjector {
    pub fn log(&self) -> Vec<Injected> {
        lock(&self.log).clone()
    }

    pub fn clear(&self) {
        lock(&self.log).clear();
    }

    /// Keys currently held according to the injected downs and ups.
    pub fn held_keys(&self) -> Vec<HidUsage> {
        let mut held = Vec::new();
        for e in lock(&self.log).iter() {
            if let Injected::Key { usage, down } = e {
                if *down {
                    if !held.contains(usage) {
                        held.push(*usage);
                    }
                } else {
                    held.retain(|k| k != usage);
                }
            }
        }
        held
    }

    /// Waits until `pred` holds for the log (or the timeout passes).
    pub async fn wait_for(
        &self,
        timeout: std::time::Duration,
        pred: impl Fn(&[Injected]) -> bool,
    ) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.changed.notified();
                if pred(&lock(&self.log)) {
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }

    fn push(&self, e: Injected) -> Result<(), PlatformError> {
        lock(&self.log).push(e);
        self.changed.notify_waiters();
        Ok(())
    }
}

impl InputInjector for MockInjector {
    fn move_abs(&self, screen: u32, x: i32, y: i32) -> Result<(), PlatformError> {
        self.push(Injected::MoveAbs { screen, x, y })
    }
    fn move_rel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        self.push(Injected::MoveRel { dx, dy })
    }
    fn key(&self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        self.push(Injected::Key { usage, down })
    }
    fn button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        self.push(Injected::Button { button, down })
    }
    fn wheel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        self.push(Injected::Wheel { dx, dy })
    }
    fn text(&self, text: &str) -> Result<(), PlatformError> {
        self.push(Injected::Text(text.to_owned()))
    }
    fn set_layout(&self, tag: &LayoutTag) -> Result<(), PlatformError> {
        self.push(Injected::Layout(tag.clone()))
    }
    fn set_locks(&self, locks: LockState) -> Result<(), PlatformError> {
        self.push(Injected::Locks(locks))
    }
}

pub struct MockScreens {
    pub screens: Mutex<Vec<LocalScreen>>,
    pub layout: Mutex<Option<LayoutTag>>,
    pub locks: Mutex<LockState>,
}

impl MockScreens {
    /// One 1920×1080 monitor at 100 %.
    pub fn single_hd() -> Self {
        Self {
            screens: Mutex::new(vec![LocalScreen {
                id: 0,
                px_w: 1920,
                px_h: 1080,
                origin_x: 0,
                origin_y: 0,
                scale_permille: 1000,
                size_mm: None,
            }]),
            layout: Mutex::new(None),
            locks: Mutex::new(LockState::default()),
        }
    }
}

impl ScreenInfo for MockScreens {
    fn screens(&self) -> Vec<LocalScreen> {
        lock(&self.screens).clone()
    }
    fn layout_tag(&self) -> Option<LayoutTag> {
        lock(&self.layout).clone()
    }
    fn locks(&self) -> LockState {
        *lock(&self.locks)
    }
}

enum Contents {
    Local(Vec<(ClipFormat, Vec<u8>)>),
    Announced(Vec<ClipFormat>, Arc<dyn ClipboardProvider>),
}

/// A clipboard the test copies into and pastes from, as a user would.
pub struct MockClipboard {
    sink: Mutex<Option<Arc<dyn ClipboardSink>>>,
    contents: Mutex<Contents>,
    announces: Mutex<u32>,
    changed: Notify,
}

impl Default for MockClipboard {
    fn default() -> Self {
        Self {
            sink: Mutex::new(None),
            contents: Mutex::new(Contents::Local(Vec::new())),
            announces: Mutex::new(0),
            changed: Notify::new(),
        }
    }
}

impl MockClipboard {
    /// A local application copies `items`.
    pub fn copy(&self, items: &[(ClipFormat, &[u8])], concealed: bool) {
        let items: Vec<_> = items.iter().map(|(f, d)| (*f, d.to_vec())).collect();
        let formats = items.iter().map(|(f, _)| *f).collect();
        *lock(&self.contents) = Contents::Local(items);
        if let Some(s) = lock(&self.sink).clone() {
            s.changed(ClipChange {
                formats,
                own: false,
                concealed,
            });
        }
    }

    /// A local application pastes `format`; blocks while a remote provider fetches it.
    pub fn paste(&self, format: ClipFormat) -> Option<Vec<u8>> {
        let provider = match &*lock(&self.contents) {
            Contents::Local(items) => {
                return items
                    .iter()
                    .find(|(f, _)| *f == format)
                    .map(|(_, d)| d.clone());
            }
            Contents::Announced(formats, p) => formats.contains(&format).then(|| p.clone())?,
        };
        provider.render(format)
    }

    pub fn announced_formats(&self) -> Option<Vec<ClipFormat>> {
        match &*lock(&self.contents) {
            Contents::Announced(f, _) => Some(f.clone()),
            Contents::Local(_) => None,
        }
    }

    pub fn announce_count(&self) -> u32 {
        *lock(&self.announces)
    }

    /// Waits until a remote offer is announced with `format` (or the timeout passes).
    pub async fn wait_announced(&self, timeout: std::time::Duration, format: ClipFormat) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.changed.notified();
                if self
                    .announced_formats()
                    .is_some_and(|f| f.contains(&format))
                {
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }
}

impl Clipboard for MockClipboard {
    fn start(&self, sink: Arc<dyn ClipboardSink>) -> Result<(), PlatformError> {
        *lock(&self.sink) = Some(sink);
        Ok(())
    }

    fn read(&self, format: ClipFormat) -> Result<Option<Vec<u8>>, PlatformError> {
        match &*lock(&self.contents) {
            Contents::Local(items) => Ok(items
                .iter()
                .find(|(f, _)| *f == format)
                .map(|(_, d)| d.clone())),
            Contents::Announced(..) => Ok(None),
        }
    }

    fn announce(
        &self,
        formats: &[ClipFormat],
        provider: Arc<dyn ClipboardProvider>,
    ) -> Result<(), PlatformError> {
        *lock(&self.contents) = Contents::Announced(formats.to_vec(), provider);
        *lock(&self.announces) += 1;
        self.changed.notify_waiters();
        // Real OSes report our own takeover as a change too.
        if let Some(s) = lock(&self.sink).clone() {
            s.changed(ClipChange {
                formats: formats.to_vec(),
                own: true,
                concealed: false,
            });
        }
        Ok(())
    }

    fn stop(&self) {
        *lock(&self.sink) = None;
    }
}

/// The test plays the user dragging files to an edge.
#[derive(Default)]
pub struct MockDragSource {
    sink: Mutex<Option<Arc<dyn DragSink>>>,
    holds: Mutex<u32>,
    finishes: Mutex<u32>,
}

impl MockDragSource {
    pub fn drag_to_edge(&self, paths: &[std::path::PathBuf]) {
        if let Some(s) = lock(&self.sink).clone() {
            s.drag_entered(paths.to_vec());
        }
    }

    pub fn drag_away(&self) {
        if let Some(s) = lock(&self.sink).clone() {
            s.drag_left();
        }
    }

    pub fn holds(&self) -> u32 {
        *lock(&self.holds)
    }

    pub fn finishes(&self) -> u32 {
        *lock(&self.finishes)
    }
}

impl DragSource for MockDragSource {
    fn start(&self, sink: Arc<dyn DragSink>) -> Result<(), PlatformError> {
        *lock(&self.sink) = Some(sink);
        Ok(())
    }
    fn hold(&self) {
        *lock(&self.holds) += 1;
    }
    fn finish(&self) {
        *lock(&self.finishes) += 1;
    }
    fn stop(&self) {
        *lock(&self.sink) = None;
    }
}

type Session = (Vec<DropItem>, Arc<dyn DropProgress>, Arc<dyn DropSink>);

/// Records OS drag sessions; the test drops or cancels them.
#[derive(Default)]
pub struct MockDropTarget {
    sessions: Mutex<Vec<Session>>,
    changed: Notify,
}

impl MockDropTarget {
    pub fn sessions(&self) -> Vec<Vec<DropItem>> {
        lock(&self.sessions).iter().map(|s| s.0.clone()).collect()
    }

    pub async fn wait_session(&self, timeout: std::time::Duration) -> Option<Vec<DropItem>> {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.changed.notified();
                if let Some(s) = lock(&self.sessions).last() {
                    return s.0.clone();
                }
                notified.await;
            }
        })
        .await
        .ok()
    }

    /// The target application takes the drop, reading the files as a real one
    /// would: it waits until they are complete. Blocks.
    pub fn drop_and_wait(&self, accepted: bool, timeout: std::time::Duration) -> bool {
        let Some((_, progress, sink)) = lock(&self.sessions).last().cloned() else {
            return false;
        };
        let complete = !accepted || progress.wait_complete(timeout);
        sink.dropped(accepted);
        complete
    }
}

impl DropTarget for MockDropTarget {
    fn begin(
        &self,
        items: Vec<DropItem>,
        progress: Arc<dyn DropProgress>,
        sink: Arc<dyn DropSink>,
    ) -> Result<(), PlatformError> {
        lock(&self.sessions).push((items, progress, sink));
        self.changed.notify_waiters();
        Ok(())
    }
}

/// A full mock platform plus typed handles to drive and inspect it.
pub struct MockPlatform {
    pub capture: Arc<MockCapture>,
    pub injector: Arc<MockInjector>,
    pub screens: Arc<MockScreens>,
    pub clipboard: Arc<MockClipboard>,
    pub drag_source: Arc<MockDragSource>,
    pub drop_target: Arc<MockDropTarget>,
}

impl MockPlatform {
    pub fn new() -> Self {
        Self {
            capture: Arc::new(MockCapture::default()),
            injector: Arc::new(MockInjector::default()),
            screens: Arc::new(MockScreens::single_hd()),
            clipboard: Arc::new(MockClipboard::default()),
            drag_source: Arc::new(MockDragSource::default()),
            drop_target: Arc::new(MockDropTarget::default()),
        }
    }

    pub fn platform(&self) -> Platform {
        Platform {
            capture: self.capture.clone(),
            injector: self.injector.clone(),
            screens: self.screens.clone(),
        }
    }
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Swallow;
    impl CaptureSink for Swallow {
        fn on_event(&self, _ev: CaptureEvent) -> Decision {
            Decision::Swallow
        }
    }

    #[tokio::test]
    async fn mock_capture_and_injector_record_everything() {
        let p = MockPlatform::new();
        assert_eq!(
            p.capture.emit(CaptureEvent::Wheel { dx: 0, dy: 1 }),
            Decision::Pass,
            "not started"
        );
        p.capture.start(Arc::new(Swallow)).unwrap();
        assert!(p.capture.is_started());
        assert_eq!(
            p.capture.emit(CaptureEvent::Wheel { dx: 0, dy: 1 }),
            Decision::Swallow
        );
        p.capture.set_grab(true);
        assert!(p.capture.is_grabbed());
        p.capture.warp(3, 4);
        assert_eq!(p.capture.warps(), vec![(3, 4)]);

        let inj = p.injector.clone();
        let waiter = tokio::spawn(async move {
            inj.wait_for(std::time::Duration::from_secs(2), |log| log.len() >= 2)
                .await
        });
        p.injector.key(HidUsage::A, true).unwrap();
        p.injector.key(HidUsage::A, false).unwrap();
        assert!(waiter.await.unwrap());
        assert!(p.injector.held_keys().is_empty());
        p.injector.key(HidUsage::Q, true).unwrap();
        assert_eq!(p.injector.held_keys(), vec![HidUsage::Q]);
        assert_eq!(p.screens.screens().len(), 1);
        p.capture.stop();
        assert!(!p.capture.is_started());
    }
}
