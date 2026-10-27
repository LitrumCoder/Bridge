//! Shared clipboard with lazily rendered formats (ARCHITECTURE §8.2, ADR-0007).
//!
//! A local copy becomes an `Offer` to every connected peer that allows us; small text
//! formats ride along, the rest is promised to the receiver's OS clipboard and fetched
//! with `Request` only when something is pasted. The paste blocks the OS clipboard
//! thread (never the node thread) until the chunks have arrived.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use bridge_core::{
    Feature, FeatureCx, FeatureError, FeatureInfo, LocalPost, PeerDownReason, PeerId,
};
use bridge_platform::{ClipChange, ClipFormat, Clipboard, ClipboardProvider, ClipboardSink};
use bridge_protocol::clipboard::{
    CHUNK_LIMIT, ClipboardMessage, Data, INLINE_LIMIT, Offer, Offered, Request, Unavailable,
};
use bridge_protocol::control::DeviceState;
use bridge_protocol::{BoundedBytes, BoundedVec, ChannelClass, FeatureId, PermissionId};
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub struct ClipboardConfig {
    pub enabled: bool,
    /// Largest format we accept from a peer (images, long texts).
    pub max_format_bytes: u64,
    /// How long a paste may wait for the data.
    pub fetch_timeout: Duration,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_format_bytes: 64 * 1024 * 1024,
            fetch_timeout: Duration::from_secs(15),
        }
    }
}

/// Formats offered to peers. `FILES` waits for the files engine (TODO(M6a)).
const SHARED: [ClipFormat; 4] = [
    ClipFormat::TEXT,
    ClipFormat::HTML,
    ClipFormat::RTF,
    ClipFormat::PNG,
];

fn is_text(f: ClipFormat) -> bool {
    matches!(f, ClipFormat::TEXT | ClipFormat::HTML | ClipFormat::RTF)
}

type Reply = Sender<Option<Vec<u8>>>;

enum Local {
    Changed(ClipChange),
    Fetch {
        peer: PeerId,
        seq: u64,
        format: ClipFormat,
        reply: Reply,
    },
    ReadDone {
        peer: PeerId,
        seq: u64,
        format: ClipFormat,
        data: Option<Vec<u8>>,
    },
}

struct Sink(LocalPost);

impl ClipboardSink for Sink {
    fn changed(&self, change: ClipChange) {
        (self.0)(Box::new(Local::Changed(change)));
    }
}

/// Renders a peer's offer for the local OS clipboard.
struct RemoteProvider {
    peer: PeerId,
    seq: u64,
    inline: HashMap<ClipFormat, Vec<u8>>,
    post: LocalPost,
    timeout: Duration,
}

impl ClipboardProvider for RemoteProvider {
    fn render(&self, format: ClipFormat) -> Option<Vec<u8>> {
        if let Some(d) = self.inline.get(&format) {
            return Some(d.clone());
        }
        let (reply, rx) = std::sync::mpsc::channel();
        (self.post)(Box::new(Local::Fetch {
            peer: self.peer,
            seq: self.seq,
            format,
            reply,
        }));
        rx.recv_timeout(self.timeout).ok().flatten()
    }
}

struct Fetching {
    buf: Vec<u8>,
    replies: Vec<Reply>,
}

pub struct ClipboardFeature {
    clipboard: Arc<dyn Clipboard>,
    post: LocalPost,
    config: ClipboardConfig,
    /// Our latest offer and what it promised.
    seq: u64,
    offered: Vec<ClipFormat>,
    cached: HashMap<ClipFormat, Arc<Vec<u8>>>,
    /// The peer offer currently in our OS clipboard.
    remote: Option<(PeerId, u64)>,
    fetching: HashMap<(PeerId, u64, ClipFormat), Fetching>,
}

impl ClipboardFeature {
    /// Starts watching the local clipboard; `post` must reach this feature's `on_local`.
    pub fn new(
        clipboard: Arc<dyn Clipboard>,
        config: ClipboardConfig,
        post: LocalPost,
    ) -> Result<Self, bridge_platform::PlatformError> {
        clipboard.start(Arc::new(Sink(post.clone())))?;
        Ok(Self {
            clipboard,
            post,
            config,
            seq: 0,
            offered: Vec::new(),
            cached: HashMap::new(),
            remote: None,
            fetching: HashMap::new(),
        })
    }

    fn targets(cx: &FeatureCx<'_>) -> Vec<PeerId> {
        cx.peers()
            .iter()
            .filter(|p| p.allows_us && p.state == DeviceState::Connected)
            .map(|p| p.id)
            .collect()
    }

    fn send(cx: &mut FeatureCx<'_>, peer: PeerId, msg: ClipboardMessage) {
        match msg.to_envelope() {
            Ok(env) => cx.send_encoded(peer, env.kind, env.payload),
            Err(e) => warn!("cannot encode a clipboard message: {e}"),
        }
    }

    fn local_changed(&mut self, cx: &mut FeatureCx<'_>, change: ClipChange) {
        if change.own || !self.config.enabled {
            return;
        }
        self.seq += 1;
        self.remote = None;
        self.cached.clear();
        self.offered.clear();
        if change.concealed {
            debug!("clipboard contents are marked as concealed; not shared");
            return;
        }
        let mut formats = Vec::new();
        let mut budget = INLINE_LIMIT;
        for f in SHARED.into_iter().filter(|f| change.formats.contains(f)) {
            if !is_text(f) {
                formats.push(Offered {
                    format: f,
                    size: 0,
                    inline: None,
                });
                continue;
            }
            let data = match self.clipboard.read(f) {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    warn!(?f, "cannot read the clipboard: {e}");
                    continue;
                }
            };
            let size = data.len() as u64;
            if data.len() <= budget {
                budget -= data.len();
                formats.push(Offered {
                    format: f,
                    size,
                    inline: BoundedBytes::new(data),
                });
            } else {
                self.cached.insert(f, Arc::new(data));
                formats.push(Offered {
                    format: f,
                    size,
                    inline: None,
                });
            }
        }
        if formats.is_empty() {
            return;
        }
        self.offered = formats.iter().map(|o| o.format).collect();
        let Ok(formats) = BoundedVec::try_from_vec(formats) else {
            return;
        };
        let offer = Offer {
            seq: self.seq,
            formats,
        };
        for peer in Self::targets(cx) {
            Self::send(cx, peer, offer.clone().into());
        }
    }

    fn remote_offer(&mut self, peer: PeerId, offer: Offer) {
        if !self.config.enabled {
            return;
        }
        let mut formats = Vec::new();
        let mut inline = HashMap::new();
        for o in offer.formats.into_vec() {
            if !SHARED.contains(&o.format) || o.size > self.config.max_format_bytes {
                continue;
            }
            formats.push(o.format);
            if let Some(d) = o.inline {
                inline.insert(o.format, d.into_vec());
            }
        }
        self.fail_fetches(|(p, _, _)| *p == peer);
        if formats.is_empty() {
            return;
        }
        let provider = RemoteProvider {
            peer,
            seq: offer.seq,
            inline,
            post: self.post.clone(),
            timeout: self.config.fetch_timeout,
        };
        match self.clipboard.announce(&formats, Arc::new(provider)) {
            Ok(()) => self.remote = Some((peer, offer.seq)),
            Err(e) => warn!("cannot put the peer's clipboard here: {e}"),
        }
    }

    fn fail_fetches(&mut self, pred: impl Fn(&(PeerId, u64, ClipFormat)) -> bool) {
        let gone: Vec<_> = self.fetching.keys().filter(|k| pred(k)).copied().collect();
        for k in gone {
            if let Some(f) = self.fetching.remove(&k) {
                for r in f.replies {
                    let _ = r.send(None);
                }
            }
        }
    }

    fn fetch(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        seq: u64,
        format: ClipFormat,
        reply: Reply,
    ) {
        if self.remote != Some((peer, seq)) {
            let _ = reply.send(None);
            return;
        }
        let key = (peer, seq, format);
        if let Some(f) = self.fetching.get_mut(&key) {
            f.replies.push(reply);
            return;
        }
        self.fetching.insert(
            key,
            Fetching {
                buf: Vec::new(),
                replies: vec![reply],
            },
        );
        Self::send(cx, peer, Request { seq, format }.into());
    }

    fn serve(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, req: Request) {
        if req.seq != self.seq || !self.offered.contains(&req.format) {
            Self::send(
                cx,
                peer,
                Unavailable {
                    seq: req.seq,
                    format: req.format,
                }
                .into(),
            );
            return;
        }
        if let Some(data) = self.cached.get(&req.format).cloned() {
            Self::send_chunks(cx, peer, req.seq, req.format, &data);
            return;
        }
        // Images can take a while to read and encode; keep the node thread free.
        let clipboard = self.clipboard.clone();
        let post = self.post.clone();
        let (seq, format) = (req.seq, req.format);
        let spawned = std::thread::Builder::new()
            .name("bridge-clipboard-read".into())
            .spawn(move || {
                let data = clipboard.read(format).ok().flatten();
                post(Box::new(Local::ReadDone {
                    peer,
                    seq,
                    format,
                    data,
                }));
            });
        if spawned.is_err() {
            Self::send(cx, peer, Unavailable { seq, format }.into());
        }
    }

    fn send_chunks(
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        seq: u64,
        format: ClipFormat,
        data: &[u8],
    ) {
        let total = data.len() as u64;
        if data.is_empty() {
            Self::send(
                cx,
                peer,
                Data {
                    seq,
                    format,
                    offset: 0,
                    total,
                    bytes: BoundedBytes::default(),
                }
                .into(),
            );
            return;
        }
        for (i, chunk) in data.chunks(CHUNK_LIMIT).enumerate() {
            let Some(bytes) = BoundedBytes::new(chunk.to_vec()) else {
                return;
            };
            let offset = (i * CHUNK_LIMIT) as u64;
            Self::send(
                cx,
                peer,
                Data {
                    seq,
                    format,
                    offset,
                    total,
                    bytes,
                }
                .into(),
            );
        }
    }

    fn data(&mut self, peer: PeerId, d: Data) -> Result<(), FeatureError> {
        let key = (peer, d.seq, d.format);
        let Some(f) = self.fetching.get_mut(&key) else {
            return Ok(());
        };
        if d.total > self.config.max_format_bytes {
            self.fail_fetches(|k| *k == key);
            return Ok(());
        }
        if d.offset != f.buf.len() as u64 {
            return Err(FeatureError::Violation(
                "clipboard chunks out of order".into(),
            ));
        }
        if f.buf.is_empty() {
            f.buf.reserve_exact(d.total as usize);
        }
        f.buf.extend_from_slice(&d.bytes);
        if f.buf.len() as u64 == d.total
            && let Some(done) = self.fetching.remove(&key)
        {
            for r in done.replies {
                let _ = r.send(Some(done.buf.clone()));
            }
        }
        Ok(())
    }
}

impl Feature for ClipboardFeature {
    fn info(&self) -> FeatureInfo {
        FeatureInfo {
            id: FeatureId::CLIPBOARD,
            name: "clipboard",
            versions: 1..=1,
            channel: ChannelClass::Clipboard,
            permission: Some(PermissionId::CLIPBOARD),
        }
    }

    fn on_peer_up(&mut self, _cx: &mut FeatureCx<'_>, _peer: PeerId, _version: u16) {}

    fn on_peer_down(&mut self, _cx: &mut FeatureCx<'_>, peer: PeerId, _reason: PeerDownReason) {
        self.fail_fetches(|(p, _, _)| *p == peer);
    }

    fn on_message(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        match ClipboardMessage::decode(kind, payload)? {
            ClipboardMessage::Offer(o) => self.remote_offer(peer, o),
            ClipboardMessage::Request(r) => self.serve(cx, peer, r),
            ClipboardMessage::Data(d) => self.data(peer, d)?,
            ClipboardMessage::Unavailable(u) => {
                self.fail_fetches(|k| *k == (peer, u.seq, u.format))
            }
        }
        Ok(())
    }

    fn on_local(&mut self, cx: &mut FeatureCx<'_>, event: Box<dyn std::any::Any + Send>) {
        let Ok(event) = event.downcast::<Local>() else {
            return;
        };
        match *event {
            Local::Changed(change) => self.local_changed(cx, change),
            Local::Fetch {
                peer,
                seq,
                format,
                reply,
            } => self.fetch(cx, peer, seq, format, reply),
            Local::ReadDone {
                peer,
                seq,
                format,
                data,
            } => match data {
                Some(d) if seq == self.seq => Self::send_chunks(cx, peer, seq, format, &d),
                _ => Self::send(cx, peer, Unavailable { seq, format }.into()),
            },
        }
    }
}

impl Drop for ClipboardFeature {
    fn drop(&mut self) {
        self.clipboard.stop();
        self.fail_fetches(|_| true);
    }
}
