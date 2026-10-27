//! File transfers (ARCHITECTURE §8.3): offers on the control stream, bytes on the bulk
//! connection, resume after reconnects, BLAKE3 over every file, validated paths,
//! conflict renaming, cancellation and source mtimes.
//!
//! The receiver drives: it asks for each file with `Want { offset }` (a few at a
//! time), the sender streams it and reports the hash with `ItemDone`. File I/O runs in
//! tokio tasks; only events come back to this state machine on the node thread.

mod fsops;
mod stream;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::{
    Feature, FeatureCx, FeatureError, FeatureInfo, LocalPost, Micros, PeerDownReason, PeerId,
};
use bridge_platform::{DragSink, DragSource, DropItem, DropProgress, DropSink, DropTarget};
use bridge_protocol::files::{
    BulkHeader, Cancel, Decline, DeclineReason, DragOffer, DragResult, FilesMessage, Finished,
    ITEMS_PER_MESSAGE, Item, ItemDone, ItemKind, Items, Offer, Restart, TransferId, Want,
};
use bridge_protocol::{BoundedVec, ChannelClass, FeatureId, PermissionId};
use bridge_transport::{BulkIncoming, BulkLink};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

pub use fsops::Conflict;
use fsops::{Walked, destinations, discard_partial, finish, resume_offset, walk};
use stream::{Expect, SendOutcome, read_header, receive_file, send_file};

#[derive(Clone, Debug)]
pub struct FilesConfig {
    pub download_dir: PathBuf,
    pub conflict: Conflict,
    /// Largest incoming transfer we accept.
    pub max_bytes: u64,
    /// Files requested at once within one transfer.
    pub parallel: usize,
    /// A requested file whose stream has not arrived by then is asked for again.
    pub stall: Duration,
    /// Where dragged files land while the drop target reads them (ADR-0008).
    pub staging_dir: PathBuf,
}

impl FilesConfig {
    pub fn new(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            conflict: Conflict::Rename,
            max_bytes: 1 << 40,
            parallel: 4,
            stall: Duration::from_secs(30),
            staging_dir: std::env::temp_dir().join("bridge-staging"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TransferState {
    Running,
    Done,
    Failed(String),
    Cancelled,
    Declined(DeclineReason),
}

impl TransferState {
    pub fn is_final(&self) -> bool {
        *self != Self::Running
    }
}

#[derive(Clone, Debug)]
pub struct TransferStatus {
    pub id: TransferId,
    pub peer: PeerId,
    pub direction: Direction,
    pub items: u32,
    pub bytes: u64,
    pub done_bytes: u64,
    pub state: TransferState,
    /// Bytes that were not sent again thanks to resuming (incoming side).
    pub resumed_bytes: u64,
    /// Top-level names, for display.
    pub names: Vec<String>,
}

type StatusMap = Arc<Mutex<HashMap<TransferId, (TransferStatus, Arc<AtomicU64>)>>>;

/// Commands and status for the IPC API and the UI.
#[derive(Clone)]
pub struct FilesHandle {
    post: LocalPost,
    status: StatusMap,
}

impl FilesHandle {
    pub async fn send(&self, peer: PeerId, paths: Vec<PathBuf>) -> Result<TransferId, String> {
        let (reply, rx) = oneshot::channel();
        (self.post)(Box::new(Local::Send { peer, paths, reply }));
        rx.await.map_err(|_| "the node stopped".to_owned())?
    }

    /// The input router moved the cursor (wired from the input feature).
    pub fn cursor(&self, event: CursorEvent) {
        (self.post)(Box::new(Local::Cursor(event)));
    }

    pub fn cancel(&self, transfer: TransferId) {
        (self.post)(Box::new(Local::Cancel(transfer)));
    }

    pub fn transfers(&self) -> Vec<TransferStatus> {
        let map = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<TransferStatus> = map
            .values()
            .map(|(s, p)| {
                let mut s = s.clone();
                s.done_bytes = p.load(Ordering::Relaxed).min(s.bytes);
                if s.state == TransferState::Done {
                    s.done_bytes = s.bytes;
                }
                s
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }

    pub fn get(&self, id: TransferId) -> Option<TransferStatus> {
        self.transfers().into_iter().find(|s| s.id == id)
    }

    /// Waits until the transfer reaches a final state.
    pub async fn wait(&self, id: TransferId, timeout: Duration) -> Option<TransferStatus> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(s) = self.get(id).filter(|s| s.state.is_final()) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .ok()
    }
}

enum Local {
    Send {
        peer: PeerId,
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<TransferId, String>>,
    },
    Walked {
        peer: PeerId,
        result: Result<Walked, String>,
        reply: oneshot::Sender<Result<TransferId, String>>,
    },
    Cancel(TransferId),
    DragEntered(Vec<PathBuf>),
    DragWalked(Result<Walked, String>),
    DragLeft,
    Cursor(CursorEvent),
    Dropped {
        transfer: TransferId,
        accepted: bool,
    },
    Prepared {
        transfer: TransferId,
        result: Option<Vec<Option<PathBuf>>>,
    },
    SendDone {
        transfer: TransferId,
        item: u32,
        outcome: Result<SendOutcome, String>,
    },
    RecvDone {
        transfer: TransferId,
        item: u32,
        result: Result<[u8; 32], String>,
    },
}

type ExpectMap = Arc<Mutex<HashMap<(PeerId, TransferId, u32), Expect>>>;

struct Out {
    peer: PeerId,
    items: Vec<Item>,
    sources: Vec<Option<PathBuf>>,
    tasks: HashMap<u32, AbortHandle>,
    progress: Arc<AtomicU64>,
}

#[derive(Default, Clone)]
struct ItemProgress {
    done: bool,
    wanted_at: Option<Micros>,
    local: Option<[u8; 32]>,
    remote: Option<ItemDone>,
    retries: u8,
}

struct In {
    peer: PeerId,
    drag: bool,
    total: u32,
    bytes: u64,
    items: Vec<Item>,
    dests: Option<Vec<Option<PathBuf>>>,
    state: Vec<ItemProgress>,
    inflight: HashSet<u32>,
    next: u32,
    progress: Arc<AtomicU64>,
}

const MAX_RETRIES: u8 = 5;

pub struct FilesFeature {
    config: FilesConfig,
    bulk: Arc<dyn BulkLink>,
    post: LocalPost,
    rt: Handle,
    status: StatusMap,
    expects: ExpectMap,
    outgoing: HashMap<TransferId, Out>,
    incoming: HashMap<TransferId, In>,
    counter: u64,
    dispatcher: AbortHandle,
    drag: Option<DragWiring>,
    pending_drag: Option<Walked>,
    active_drag: Option<TransferId>,
}

impl FilesFeature {
    /// Must be created inside a tokio runtime; `post` must reach this feature.
    pub fn new(
        config: FilesConfig,
        bulk: Arc<dyn BulkLink>,
        mut streams: BulkIncoming,
        post: LocalPost,
    ) -> (Self, FilesHandle) {
        let rt = Handle::current();
        let status = StatusMap::default();
        let expects = ExpectMap::default();
        let (ex, p) = (expects.clone(), post.clone());
        let dispatcher = rt
            .spawn(async move {
                while let Some((peer, mut s)) = streams.recv().await {
                    let (ex, p) = (ex.clone(), p.clone());
                    tokio::spawn(async move {
                        let Ok(h) = read_header(&mut *s).await else {
                            s.stop(1);
                            return;
                        };
                        let key = (peer, h.transfer, h.item);
                        let expect = ex.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                        let Some(expect) = expect.filter(|e| e.offset == h.offset) else {
                            s.stop(1);
                            return;
                        };
                        let result = receive_file(&mut *s, &expect).await;
                        p(Box::new(Local::RecvDone {
                            transfer: h.transfer,
                            item: h.item,
                            result,
                        }));
                    });
                }
            })
            .abort_handle();
        let handle = FilesHandle {
            post: post.clone(),
            status: status.clone(),
        };
        (
            Self {
                config,
                bulk,
                post,
                rt,
                status,
                expects,
                outgoing: HashMap::new(),
                incoming: HashMap::new(),
                counter: 0,
                drag: None,
                pending_drag: None,
                active_drag: None,
                dispatcher,
            },
            handle,
        )
    }

    fn send(cx: &mut FeatureCx<'_>, peer: PeerId, msg: FilesMessage) {
        match msg.to_envelope() {
            Ok(env) => cx.send_encoded(peer, env.kind, env.payload),
            Err(e) => warn!("cannot encode a files message: {e}"),
        }
    }

    /// Enables dragging files across the edges (ADR-0008).
    pub fn with_drag(mut self, wiring: DragWiring) -> Self {
        if let Err(e) = wiring.source.start(Arc::new(DragEvents(self.post.clone()))) {
            warn!("dragging files out of this device is unavailable: {e}");
        }
        clean_staging(&self.config.staging_dir);
        self.drag = Some(wiring);
        self
    }

    fn set_drag_active(&self, active: bool) {
        if let Some(d) = &self.drag {
            (d.set_drag_active)(active);
        }
    }

    fn end_drag(&mut self) {
        if self.active_drag.take().is_some() {
            if let Some(d) = &self.drag {
                d.source.finish();
            }
            self.set_drag_active(false);
        }
    }

    fn set_state(&self, id: TransferId, state: TransferState) {
        if let Some((s, _)) = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&id)
        {
            s.state = state;
        }
    }

    fn new_id(&mut self, peer: PeerId, paths: &[PathBuf]) -> TransferId {
        self.counter += 1;
        let mut h = blake3::Hasher::new();
        h.update(&self.counter.to_le_bytes());
        h.update(&std::process::id().to_le_bytes());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        h.update(&nanos.to_le_bytes());
        h.update(&peer.0);
        for p in paths {
            h.update(p.as_os_str().as_encoded_bytes());
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&h.finalize().as_bytes()[..16]);
        TransferId(id)
    }

    fn start_send(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        walked: Walked,
        drag: bool,
    ) -> Result<TransferId, String> {
        let allowed = cx.peer(&peer).is_some_and(|p| p.allows_us);
        if !allowed {
            return Err("the device is not connected or does not accept files from us".into());
        }
        if walked.items.is_empty() {
            return Err("nothing to send".into());
        }
        if walked.skipped_links > 0 {
            warn!(count = walked.skipped_links, "symbolic links are not sent");
        }
        let id = self.new_id(peer, &[]);
        let bytes: u64 = walked.items.iter().map(|i| i.size).sum();
        let total = walked.items.len() as u32;
        let offer = Offer {
            transfer: id,
            items: total,
            bytes,
        };
        let msg = if drag {
            DragOffer { offer }.into()
        } else {
            offer.into()
        };
        Self::send(cx, peer, msg);
        for (n, chunk) in walked.items.chunks(ITEMS_PER_MESSAGE).enumerate() {
            let Ok(items) = BoundedVec::try_from_vec(chunk.to_vec()) else {
                continue;
            };
            let first = (n * ITEMS_PER_MESSAGE) as u32;
            Self::send(
                cx,
                peer,
                Items {
                    transfer: id,
                    first,
                    items,
                }
                .into(),
            );
        }
        let progress = Arc::new(AtomicU64::new(0));
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                id,
                (
                    TransferStatus {
                        id,
                        peer,
                        direction: Direction::Outgoing,
                        items: total,
                        bytes,
                        done_bytes: 0,
                        state: TransferState::Running,
                        resumed_bytes: 0,
                        names: top_names(&walked.items),
                    },
                    progress.clone(),
                ),
            );
        self.outgoing.insert(
            id,
            Out {
                peer,
                items: walked.items,
                sources: walked.sources,
                tasks: HashMap::new(),
                progress,
            },
        );
        info!(%id, %peer, total, bytes, "sending files");
        Ok(id)
    }

    fn on_want(&mut self, peer: PeerId, w: Want) {
        let Some(out) = self
            .outgoing
            .get_mut(&w.transfer)
            .filter(|o| o.peer == peer)
        else {
            return;
        };
        let i = w.item as usize;
        let (Some(item), Some(Some(source))) = (out.items.get(i), out.sources.get(i)) else {
            return;
        };
        if let Some(t) = out.tasks.remove(&w.item) {
            t.abort();
        }
        let (bulk, post, progress) = (self.bulk.clone(), self.post.clone(), out.progress.clone());
        let source = source.clone();
        let expect = (item.size, item.mtime_ms);
        let header = BulkHeader {
            transfer: w.transfer,
            item: w.item,
            offset: w.offset,
        };
        let task = self.rt.spawn(async move {
            let outcome = async {
                let mut s = ResetOnDrop(Some(bulk.open(peer).await.map_err(|e| e.to_string())?));
                let r = send_file(s.stream(), header, &source, expect, &progress).await;
                if r.is_ok() {
                    s.0 = None;
                }
                r
            }
            .await;
            post(Box::new(Local::SendDone {
                transfer: w.transfer,
                item: w.item,
                outcome,
            }));
        });
        out.tasks.insert(w.item, task.abort_handle());
    }

    fn on_send_done(
        &mut self,
        cx: &mut FeatureCx<'_>,
        transfer: TransferId,
        item: u32,
        outcome: Result<SendOutcome, String>,
    ) {
        let Some(out) = self.outgoing.get_mut(&transfer) else {
            return;
        };
        out.tasks.remove(&item);
        let peer = out.peer;
        match outcome {
            Ok(SendOutcome::Done(sent)) => {
                Self::send(
                    cx,
                    peer,
                    ItemDone {
                        transfer,
                        item,
                        size: sent.size,
                        mtime_ms: sent.mtime_ms,
                        blake3: sent.hash,
                    }
                    .into(),
                );
            }
            Ok(SendOutcome::Changed(now)) => {
                if let Some(it) = out.items.get_mut(item as usize)
                    && let Some((size, mtime)) = now
                {
                    it.size = size;
                    it.mtime_ms = mtime;
                }
                let msg = Restart {
                    transfer,
                    item,
                    size: now.map(|n| n.0),
                    mtime_ms: now.map_or(0, |n| n.1),
                };
                Self::send(cx, peer, msg.into());
            }
            // The receiver notices the missing stream and asks again.
            Err(e) => debug!(%transfer, item, "sending failed: {e}"),
        }
    }

    fn on_offer(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, o: Offer, drag: bool) {
        if self.incoming.contains_key(&o.transfer) || self.outgoing.contains_key(&o.transfer) {
            return;
        }
        if o.bytes > self.config.max_bytes {
            Self::send(
                cx,
                peer,
                Decline {
                    transfer: o.transfer,
                    reason: DeclineReason::TooLarge,
                }
                .into(),
            );
            return;
        }
        let progress = Arc::new(AtomicU64::new(0));
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                o.transfer,
                (
                    TransferStatus {
                        id: o.transfer,
                        peer,
                        direction: Direction::Incoming,
                        items: o.items,
                        bytes: o.bytes,
                        done_bytes: 0,
                        state: TransferState::Running,
                        resumed_bytes: 0,
                        names: Vec::new(),
                    },
                    progress.clone(),
                ),
            );
        self.incoming.insert(
            o.transfer,
            In {
                peer,
                drag,
                total: o.items,
                bytes: o.bytes,
                items: Vec::new(),
                dests: None,
                state: Vec::new(),
                inflight: HashSet::new(),
                next: 0,
                progress,
            },
        );
    }

    fn on_items(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, m: Items) {
        let Some(t) = self
            .incoming
            .get_mut(&m.transfer)
            .filter(|t| t.peer == peer)
        else {
            return;
        };
        if m.first as usize != t.items.len() || t.items.len() + m.items.len() > t.total as usize {
            self.decline(cx, m.transfer, DeclineReason::BadItems);
            return;
        }
        t.items.extend(m.items.into_vec());
        if t.items.len() < t.total as usize {
            return;
        }
        let sum: u64 = t.items.iter().map(|i| i.size).sum();
        if sum != t.bytes {
            self.decline(cx, m.transfer, DeclineReason::BadItems);
            return;
        }
        if let Some((s, _)) = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&m.transfer)
        {
            s.names = top_names(&t.items);
        }
        let base = if t.drag {
            self.config.staging_dir.join(m.transfer.to_string())
        } else {
            self.config.download_dir.clone()
        };
        let (items, policy, post) = (t.items.clone(), self.config.conflict, self.post.clone());
        let transfer = m.transfer;
        self.rt.spawn_blocking(move || {
            let result = std::fs::create_dir_all(&base)
                .ok()
                .and_then(|()| destinations(&base, &items, policy))
                .and_then(|dests| {
                    for (item, dest) in items.iter().zip(&dests) {
                        if let (ItemKind::Dir, Some(d)) = (item.kind, dest) {
                            std::fs::create_dir_all(d).ok()?;
                        }
                    }
                    Some(dests)
                });
            post(Box::new(Local::Prepared { transfer, result }));
        });
    }

    fn decline(&mut self, cx: &mut FeatureCx<'_>, transfer: TransferId, reason: DeclineReason) {
        if let Some(t) = self.incoming.remove(&transfer) {
            Self::send(cx, t.peer, Decline { transfer, reason }.into());
            self.set_state(transfer, TransferState::Declined(reason));
        }
    }

    fn on_prepared(
        &mut self,
        cx: &mut FeatureCx<'_>,
        transfer: TransferId,
        result: Option<Vec<Option<PathBuf>>>,
    ) {
        let Some(t) = self.incoming.get_mut(&transfer) else {
            return;
        };
        let Some(dests) = result else {
            self.decline(cx, transfer, DeclineReason::BadItems);
            return;
        };
        t.state = t
            .items
            .iter()
            .zip(&dests)
            .map(|(item, d)| ItemProgress {
                done: item.kind == ItemKind::Dir || d.is_none(),
                ..ItemProgress::default()
            })
            .collect();
        if t.drag {
            let top: Vec<DropItem> = t
                .items
                .iter()
                .zip(&dests)
                .filter(|(i, _)| i.path.len() == 1)
                .filter_map(|(i, d)| {
                    d.clone().map(|path| DropItem {
                        path,
                        is_dir: i.kind == ItemKind::Dir,
                    })
                })
                .collect();
            let peer = t.peer;
            let progress = Arc::new(TransferProgress {
                status: self.status.clone(),
                id: transfer,
            });
            let sink = Arc::new(DropDone {
                post: self.post.clone(),
                transfer,
            });
            let begun = self
                .drag
                .as_ref()
                .and_then(|w| w.target.as_ref())
                .map(|target| target.begin(top, progress, sink));
            if let Some(Err(e)) = begun {
                warn!(%transfer, "cannot start the drag here: {e}");
                Self::send(
                    cx,
                    peer,
                    DragResult {
                        transfer,
                        dropped: false,
                    }
                    .into(),
                );
                self.drop_incoming(transfer, true);
                self.set_state(transfer, TransferState::Cancelled);
                return;
            }
        }
        if let Some(t) = self.incoming.get_mut(&transfer) {
            t.dests = Some(dests);
        }
        self.pump(cx, transfer);
    }

    fn want(&mut self, cx: &mut FeatureCx<'_>, transfer: TransferId, item: u32) {
        let Some(t) = self.incoming.get_mut(&transfer) else {
            return;
        };
        let i = item as usize;
        let (Some(it), Some(Some(dest))) =
            (t.items.get(i), t.dests.as_ref().and_then(|d| d.get(i)))
        else {
            return;
        };
        let offset = match resume_offset(dest, transfer, it.size, it.mtime_ms) {
            Ok(o) => o,
            Err(e) => {
                let (peer, why) = (t.peer, format!("{}: {e}", dest.display()));
                self.fail_incoming(cx, transfer, peer, why);
                return;
            }
        };
        let dest = dest.clone();
        if offset > 0
            && let Some((s, _)) = self
                .status
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_mut(&transfer)
        {
            s.resumed_bytes += offset;
        }
        let st = &mut t.state[i];
        st.local = None;
        st.wanted_at = Some(cx.now());
        t.inflight.insert(item);
        self.expects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                (t.peer, transfer, item),
                Expect {
                    dest: dest.clone(),
                    offset,
                    size: it.size,
                    progress: t.progress.clone(),
                },
            );
        Self::send(
            cx,
            t.peer,
            Want {
                transfer,
                item,
                offset,
            }
            .into(),
        );
    }

    fn pump(&mut self, cx: &mut FeatureCx<'_>, transfer: TransferId) {
        let parallel = self.config.parallel.max(1);
        loop {
            let Some(t) = self.incoming.get_mut(&transfer) else {
                return;
            };
            if t.dests.is_none() {
                return;
            }
            if t.inflight.len() >= parallel || t.next >= t.total {
                break;
            }
            let item = t.next;
            t.next += 1;
            if !t.state[item as usize].done {
                self.want(cx, transfer, item);
            }
        }
        let Some(t) = self.incoming.get(&transfer) else {
            return;
        };
        if t.state.iter().all(|s| s.done) {
            let peer = t.peer;
            self.incoming.remove(&transfer);
            self.set_state(transfer, TransferState::Done);
            Self::send(cx, peer, Finished { transfer, ok: true }.into());
            info!(%transfer, "files received");
        }
    }

    fn check_item(&mut self, cx: &mut FeatureCx<'_>, transfer: TransferId, item: u32) {
        let policy = self.config.conflict;
        let Some(t) = self.incoming.get_mut(&transfer) else {
            return;
        };
        let i = item as usize;
        let st = &mut t.state[i];
        let (Some(local), Some(remote)) = (st.local, st.remote.clone()) else {
            return;
        };
        let Some(Some(dest)) = t.dests.as_ref().map(|d| d[i].clone()) else {
            return;
        };
        t.inflight.remove(&item);
        if local == remote.blake3 && remote.size == t.items[i].size {
            match finish(&dest, remote.mtime_ms, policy) {
                Ok(placed) => {
                    debug!(%transfer, item, path = %placed.display(), "file received");
                    st.done = true;
                }
                Err(e) => {
                    let peer = t.peer;
                    self.fail_incoming(cx, transfer, peer, format!("{}: {e}", dest.display()));
                    return;
                }
            }
        } else {
            warn!(%transfer, item, "hash mismatch, fetching the file again");
            discard_partial(&dest);
            st.remote = None;
            st.retries += 1;
            if st.retries > MAX_RETRIES {
                let peer = t.peer;
                self.fail_incoming(cx, transfer, peer, "the file arrives corrupted".into());
                return;
            }
            self.want(cx, transfer, item);
        }
        self.pump(cx, transfer);
    }

    fn on_recv_done(
        &mut self,
        cx: &mut FeatureCx<'_>,
        transfer: TransferId,
        item: u32,
        result: Result<[u8; 32], String>,
    ) {
        let Some(t) = self.incoming.get_mut(&transfer) else {
            return;
        };
        let Some(st) = t.state.get_mut(item as usize) else {
            return;
        };
        match result {
            Ok(hash) => {
                st.local = Some(hash);
                self.check_item(cx, transfer, item);
            }
            Err(e) => {
                // Usually a dropped connection: resume from what arrived.
                debug!(%transfer, item, "receiving failed: {e}");
                st.wanted_at = None;
                st.retries += 1;
                if st.retries > MAX_RETRIES {
                    let peer = t.peer;
                    self.fail_incoming(cx, transfer, peer, e);
                    return;
                }
                let connected = cx.peer(&t.peer).is_some();
                if connected {
                    self.want(cx, transfer, item);
                }
            }
        }
    }

    fn on_restart(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, r: Restart) {
        let Some(t) = self
            .incoming
            .get_mut(&r.transfer)
            .filter(|t| t.peer == peer)
        else {
            return;
        };
        let i = r.item as usize;
        let (Some(it), Some(st)) = (t.items.get_mut(i), t.state.get_mut(i)) else {
            return;
        };
        if let Some(Some(dest)) = t.dests.as_ref().map(|d| d[i].clone()) {
            discard_partial(&dest);
        }
        self.expects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(peer, r.transfer, r.item));
        st.local = None;
        st.remote = None;
        match r.size {
            Some(size) => {
                t.bytes = t.bytes - it.size + size;
                it.size = size;
                it.mtime_ms = r.mtime_ms;
                self.want(cx, r.transfer, r.item);
            }
            None => {
                warn!(transfer = %r.transfer, item = r.item, "the source file vanished; skipped");
                st.done = true;
                t.inflight.remove(&r.item);
                self.pump(cx, r.transfer);
            }
        }
    }

    fn fail_incoming(
        &mut self,
        cx: &mut FeatureCx<'_>,
        transfer: TransferId,
        peer: PeerId,
        why: String,
    ) {
        warn!(%transfer, "receiving files failed: {why}");
        self.drop_incoming(transfer, false);
        self.set_state(transfer, TransferState::Failed(why));
        Self::send(
            cx,
            peer,
            Finished {
                transfer,
                ok: false,
            }
            .into(),
        );
    }

    /// Forgets an incoming transfer; `discard` also removes partial files.
    fn drop_incoming(&mut self, transfer: TransferId, discard: bool) {
        let Some(t) = self.incoming.remove(&transfer) else {
            return;
        };
        self.expects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|k, _| k.1 != transfer);
        if discard && let Some(dests) = &t.dests {
            for (d, st) in dests.iter().zip(&t.state) {
                if let (Some(d), false) = (d, st.done) {
                    discard_partial(d);
                }
            }
        }
    }

    fn drop_outgoing(&mut self, transfer: TransferId) -> Option<PeerId> {
        if self.active_drag == Some(transfer) {
            self.end_drag();
        }
        let out = self.outgoing.remove(&transfer)?;
        for t in out.tasks.values() {
            t.abort();
        }
        Some(out.peer)
    }

    fn cancel_local(&mut self, cx: &mut FeatureCx<'_>, transfer: TransferId) {
        let peer = self.drop_outgoing(transfer).or_else(|| {
            let p = self.incoming.get(&transfer).map(|t| t.peer);
            self.drop_incoming(transfer, true);
            p
        });
        if let Some(peer) = peer {
            Self::send(cx, peer, Cancel { transfer }.into());
            self.set_state(transfer, TransferState::Cancelled);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorEvent {
    Entered(PeerId),
    Returned,
}

/// Drag and drop services (ADR-0008). Without a drop target (Wayland), dragged-in
/// files are saved like any other transfer.
pub struct DragWiring {
    pub source: Arc<dyn DragSource>,
    pub target: Option<Arc<dyn DropTarget>>,
    /// Tells the input router that a file drag may cross with the button held.
    pub set_drag_active: Arc<dyn Fn(bool) + Send + Sync>,
}

struct DragEvents(LocalPost);

impl DragSink for DragEvents {
    fn drag_entered(&self, paths: Vec<PathBuf>) {
        (self.0)(Box::new(Local::DragEntered(paths)));
    }
    fn drag_left(&self) {
        (self.0)(Box::new(Local::DragLeft));
    }
}

struct DropDone {
    post: LocalPost,
    transfer: TransferId,
}

impl DropSink for DropDone {
    fn dropped(&self, accepted: bool) {
        (self.post)(Box::new(Local::Dropped {
            transfer: self.transfer,
            accepted,
        }));
    }
}

/// What an OS drop target waits on before reading staged files.
struct TransferProgress {
    status: StatusMap,
    id: TransferId,
}

impl DropProgress for TransferProgress {
    fn wait_complete(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let state = self
                .status
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&self.id)
                .map(|(s, _)| s.state.clone());
            match state {
                Some(TransferState::Done) => return true,
                Some(TransferState::Running) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => return false,
            }
        }
    }
}

/// Staged drops stay for a week: the target program may keep using them in place
/// (a DAW referencing a sample, for instance).
fn clean_staging(dir: &std::path::Path) {
    let week = Duration::from_secs(7 * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > week);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Resets the stream when a send is aborted: dropping a quinn stream would finish it,
/// and the receiver would take a cut-off file for a short one.
struct ResetOnDrop(Option<Box<dyn bridge_transport::SendStream>>);

impl ResetOnDrop {
    fn stream(&mut self) -> &mut dyn bridge_transport::SendStream {
        match &mut self.0 {
            Some(s) => &mut **s,
            None => unreachable!("taken only after the send"),
        }
    }
}

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        if let Some(s) = &mut self.0 {
            s.reset(CANCELLED);
        }
    }
}

const CANCELLED: u32 = 1;

fn top_names(items: &[Item]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for i in items {
        if let Some(c) = i.path.first()
            && !names.iter().any(|n| n == c.as_str())
        {
            names.push(c.as_str().to_owned());
        }
    }
    names
}

impl Feature for FilesFeature {
    fn info(&self) -> FeatureInfo {
        FeatureInfo {
            id: FeatureId::FILES,
            name: "files",
            versions: 1..=1,
            channel: ChannelClass::Control,
            permission: Some(PermissionId::FILES),
        }
    }

    fn on_peer_up(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, _version: u16) {
        // Resume what was interrupted: each in-flight file from where it stopped.
        let resume: Vec<(TransferId, u32)> = self
            .incoming
            .iter()
            .filter(|(_, t)| t.peer == peer)
            .flat_map(|(id, t)| t.inflight.iter().map(move |i| (*id, *i)))
            .collect();
        for (transfer, item) in resume {
            self.want(cx, transfer, item);
        }
    }

    fn on_peer_down(&mut self, _cx: &mut FeatureCx<'_>, peer: PeerId, reason: PeerDownReason) {
        for out in self.outgoing.values_mut().filter(|o| o.peer == peer) {
            for (_, t) in out.tasks.drain() {
                t.abort();
            }
        }
        self.expects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|k, _| k.0 != peer);
        if matches!(reason, PeerDownReason::Revoked) {
            let gone: Vec<TransferId> = self
                .incoming
                .iter()
                .filter(|(_, t)| t.peer == peer)
                .map(|(id, _)| *id)
                .collect();
            for id in gone {
                self.drop_incoming(id, true);
                self.set_state(id, TransferState::Cancelled);
            }
        }
    }

    fn on_message(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        match FilesMessage::decode(kind, payload)? {
            FilesMessage::Offer(o) => self.on_offer(cx, peer, o, false),
            FilesMessage::DragOffer(d) => {
                let staged = self.drag.as_ref().is_some_and(|w| w.target.is_some());
                self.on_offer(cx, peer, d.offer, staged);
            }
            FilesMessage::DragResult(r) => {
                if self.active_drag == Some(r.transfer)
                    && self
                        .outgoing
                        .get(&r.transfer)
                        .is_some_and(|o| o.peer == peer)
                {
                    self.end_drag();
                    if !r.dropped {
                        self.drop_outgoing(r.transfer);
                        self.set_state(r.transfer, TransferState::Cancelled);
                    }
                }
            }
            FilesMessage::Items(m) => self.on_items(cx, peer, m),
            FilesMessage::Decline(d) => {
                if self
                    .outgoing
                    .get(&d.transfer)
                    .is_some_and(|o| o.peer == peer)
                {
                    self.drop_outgoing(d.transfer);
                    self.set_state(d.transfer, TransferState::Declined(d.reason));
                }
            }
            FilesMessage::Want(w) => self.on_want(peer, w),
            FilesMessage::ItemDone(d) => {
                let (transfer, item) = (d.transfer, d.item);
                let Some(st) = self
                    .incoming
                    .get_mut(&transfer)
                    .filter(|t| t.peer == peer)
                    .and_then(|t| t.state.get_mut(item as usize))
                else {
                    return Ok(());
                };
                st.remote = Some(d);
                self.check_item(cx, transfer, item);
            }
            FilesMessage::Restart(r) => self.on_restart(cx, peer, r),
            FilesMessage::Cancel(c) => {
                if self
                    .outgoing
                    .get(&c.transfer)
                    .is_some_and(|o| o.peer == peer)
                {
                    self.drop_outgoing(c.transfer);
                    self.set_state(c.transfer, TransferState::Cancelled);
                } else if self
                    .incoming
                    .get(&c.transfer)
                    .is_some_and(|t| t.peer == peer)
                {
                    self.drop_incoming(c.transfer, true);
                    self.set_state(c.transfer, TransferState::Cancelled);
                }
            }
            FilesMessage::Finished(f) => {
                if self
                    .outgoing
                    .get(&f.transfer)
                    .is_some_and(|o| o.peer == peer)
                {
                    self.drop_outgoing(f.transfer);
                    let state = if f.ok {
                        TransferState::Done
                    } else {
                        TransferState::Failed("the receiver gave up".into())
                    };
                    self.set_state(f.transfer, state);
                }
            }
        }
        Ok(())
    }

    fn on_local(&mut self, cx: &mut FeatureCx<'_>, event: Box<dyn std::any::Any + Send>) {
        let Ok(event) = event.downcast::<Local>() else {
            return;
        };
        match *event {
            Local::Send { peer, paths, reply } => {
                let post = self.post.clone();
                self.rt.spawn_blocking(move || {
                    let result = walk(&paths);
                    post(Box::new(Local::Walked {
                        peer,
                        result,
                        reply,
                    }));
                });
            }
            Local::Walked {
                peer,
                result,
                reply,
            } => {
                let r = result.and_then(|w| self.start_send(cx, peer, w, false));
                let _ = reply.send(r);
            }
            Local::Cancel(t) => self.cancel_local(cx, t),
            Local::DragEntered(paths) => {
                let post = self.post.clone();
                self.rt
                    .spawn_blocking(move || post(Box::new(Local::DragWalked(walk(&paths)))));
            }
            Local::DragWalked(Ok(w)) if self.active_drag.is_none() => {
                self.pending_drag = Some(w);
                self.set_drag_active(true);
            }
            Local::DragWalked(Ok(_)) => {}
            Local::DragWalked(Err(e)) => warn!("cannot drag these files to another device: {e}"),
            Local::DragLeft => {
                if self.active_drag.is_none() && self.pending_drag.take().is_some() {
                    self.set_drag_active(false);
                }
            }
            Local::Cursor(CursorEvent::Entered(peer)) => {
                if let Some(w) = self.pending_drag.take() {
                    match self.start_send(cx, peer, w, true) {
                        Ok(id) => {
                            self.active_drag = Some(id);
                            if let Some(d) = &self.drag {
                                d.source.hold();
                            }
                        }
                        Err(e) => {
                            warn!("cannot drag the files there: {e}");
                            self.set_drag_active(false);
                        }
                    }
                }
            }
            Local::Cursor(CursorEvent::Returned) => {}
            Local::Dropped { transfer, accepted } => {
                if let Some(peer) = self.incoming.get(&transfer).map(|t| t.peer).or_else(|| {
                    self.status
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&transfer)
                        .map(|(s, _)| s.peer)
                }) {
                    Self::send(
                        cx,
                        peer,
                        DragResult {
                            transfer,
                            dropped: accepted,
                        }
                        .into(),
                    );
                }
                if !accepted {
                    self.cancel_local(cx, transfer);
                }
            }
            Local::Prepared { transfer, result } => self.on_prepared(cx, transfer, result),
            Local::SendDone {
                transfer,
                item,
                outcome,
            } => self.on_send_done(cx, transfer, item, outcome),
            Local::RecvDone {
                transfer,
                item,
                result,
            } => self.on_recv_done(cx, transfer, item, result),
        }
    }

    fn on_tick(&mut self, cx: &mut FeatureCx<'_>) {
        // A request whose stream never showed up (the sender failed before opening
        // it) is sent again; streams that arrived report their own failures.
        let now = cx.now();
        let stall = self.config.stall;
        let unclaimed: Vec<(TransferId, u32)> = {
            let expects = self.expects.lock().unwrap_or_else(|p| p.into_inner());
            self.incoming
                .iter()
                .filter(|(_, t)| cx.peer(&t.peer).is_some())
                .flat_map(|(id, t)| {
                    t.inflight
                        .iter()
                        .filter(|i| {
                            let s = &t.state[**i as usize];
                            expects.contains_key(&(t.peer, *id, **i))
                                && s.wanted_at.is_some_and(|w| now.saturating_sub(w) > stall)
                        })
                        .map(move |i| (*id, *i))
                })
                .collect()
        };
        for (transfer, item) in unclaimed {
            self.want(cx, transfer, item);
        }
    }
}

impl Drop for FilesFeature {
    fn drop(&mut self) {
        self.dispatcher.abort();
        for out in self.outgoing.values() {
            for t in out.tasks.values() {
                t.abort();
            }
        }
    }
}
