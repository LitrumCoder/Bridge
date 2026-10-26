//! In-memory UDP network for quinn (ADR-0014): delay, jitter, loss, partitions and a
//! bandwidth-limited bottleneck with a finite queue, so bufferbloat is reproducible.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bridge_transport::udp::{RecvMeta, Transmit};
use bridge_transport::{AsyncUdpSocket, UdpPoller};
use tokio::time::{Instant, Sleep};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkConfig {
    pub latency: Duration,
    /// Extra uniformly distributed delay in `0..=jitter`; can reorder packets.
    pub jitter: Duration,
    /// Probability in `0.0..=1.0` that a packet is dropped.
    pub loss: f64,
    /// Bottleneck rate; `None` means unlimited.
    pub bandwidth_bps: Option<u64>,
    /// Bytes the bottleneck queue holds before tail-dropping.
    pub queue_bytes: usize,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_micros(200),
            jitter: Duration::ZERO,
            loss: 0.0,
            bandwidth_bps: None,
            queue_bytes: 1 << 20,
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NetStats {
    pub sent: u64,
    pub delivered: u64,
    pub lost: u64,
    pub queue_drops: u64,
    pub partitioned: u64,
}

struct Packet {
    from: SocketAddr,
    data: Vec<u8>,
}

#[derive(Default)]
struct Inbox {
    pending: BinaryHeap<Reverse<(Instant, u64)>>,
    packets: HashMap<u64, Packet>,
    ready: VecDeque<Packet>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct LinkState {
    busy_until: Option<Instant>,
}

struct State {
    inboxes: HashMap<SocketAddr, Inbox>,
    links: HashMap<(SocketAddr, SocketAddr), LinkConfig>,
    link_state: HashMap<(SocketAddr, SocketAddr), LinkState>,
    default_link: LinkConfig,
    cut: HashSet<(SocketAddr, SocketAddr)>,
    rng: SplitMix64,
    seq: u64,
    next_port: u16,
    stats: NetStats,
}

/// A simulated network. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct Network {
    state: Arc<Mutex<State>>,
}

impl fmt::Debug for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Network")
    }
}

impl Network {
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                inboxes: HashMap::new(),
                links: HashMap::new(),
                link_state: HashMap::new(),
                default_link: LinkConfig::default(),
                cut: HashSet::new(),
                rng: SplitMix64(seed),
                seq: 0,
                next_port: 40_000,
                stats: NetStats::default(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Creates a socket with a fresh address `10.0.0.1:<port>`.
    pub fn socket(&self) -> Arc<SimSocket> {
        let mut s = self.lock();
        let addr = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), s.next_port));
        s.next_port += 1;
        s.inboxes.insert(addr, Inbox::default());
        Arc::new(SimSocket {
            net: self.clone(),
            addr,
            timer: Mutex::new(None),
        })
    }

    pub fn set_default_link(&self, config: LinkConfig) {
        self.lock().default_link = config;
    }

    /// Configures the direction `from → to`.
    pub fn set_link(&self, from: SocketAddr, to: SocketAddr, config: LinkConfig) {
        self.lock().links.insert((from, to), config);
    }

    pub fn set_link_both(&self, a: SocketAddr, b: SocketAddr, config: LinkConfig) {
        self.set_link(a, b, config);
        self.set_link(b, a, config);
    }

    /// Drops everything between `a` and `b` in both directions.
    pub fn partition(&self, a: SocketAddr, b: SocketAddr) {
        let mut s = self.lock();
        s.cut.insert((a, b));
        s.cut.insert((b, a));
    }

    pub fn heal(&self, a: SocketAddr, b: SocketAddr) {
        let mut s = self.lock();
        s.cut.remove(&(a, b));
        s.cut.remove(&(b, a));
    }

    pub fn stats(&self) -> NetStats {
        self.lock().stats
    }

    fn send(&self, from: SocketAddr, to: SocketAddr, data: &[u8]) {
        let mut guard = self.lock();
        let s = &mut *guard;
        s.stats.sent += 1;
        if s.cut.contains(&(from, to)) {
            s.stats.partitioned += 1;
            return;
        }
        if !s.inboxes.contains_key(&to) {
            return;
        }
        let cfg = s.links.get(&(from, to)).copied().unwrap_or(s.default_link);
        if cfg.loss > 0.0 && s.rng.next_f64() < cfg.loss {
            s.stats.lost += 1;
            return;
        }
        let now = Instant::now();
        let mut depart = now;
        if let Some(bps) = cfg.bandwidth_bps.filter(|b| *b > 0) {
            let link = s.link_state.entry((from, to)).or_default();
            let start = link.busy_until.filter(|b| *b > now).unwrap_or(now);
            let queued = start.saturating_duration_since(now);
            let queued_bytes = (queued.as_secs_f64() * bps as f64 / 8.0) as usize;
            if queued_bytes + data.len() > cfg.queue_bytes {
                s.stats.queue_drops += 1;
                return;
            }
            let tx = Duration::from_secs_f64(data.len() as f64 * 8.0 / bps as f64);
            depart = start + tx;
            link.busy_until = Some(depart);
        }
        let jitter = if cfg.jitter.is_zero() {
            Duration::ZERO
        } else {
            cfg.jitter.mul_f64(s.rng.next_f64())
        };
        let at = depart + cfg.latency + jitter;
        s.seq += 1;
        let seq = s.seq;
        let Some(inbox) = s.inboxes.get_mut(&to) else {
            return;
        };
        inbox.packets.insert(
            seq,
            Packet {
                from,
                data: data.to_vec(),
            },
        );
        inbox.pending.push(Reverse((at, seq)));
        if let Some(w) = inbox.waker.take() {
            w.wake();
        }
    }
}

pub struct SimSocket {
    net: Network,
    addr: SocketAddr,
    timer: Mutex<Option<Pin<Box<Sleep>>>>,
}

impl fmt::Debug for SimSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SimSocket({})", self.addr)
    }
}

impl SimSocket {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for SimSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        let seg = transmit
            .segment_size
            .unwrap_or(transmit.contents.len())
            .max(1);
        for chunk in transmit.contents.chunks(seg) {
            self.net.send(self.addr, transmit.destination, chunk);
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let next_due = {
            let mut guard = self.net.lock();
            let s = &mut *guard;
            let Some(inbox) = s.inboxes.get_mut(&self.addr) else {
                return Poll::Ready(Err(io::ErrorKind::NotConnected.into()));
            };
            let now = Instant::now();
            while let Some(Reverse((at, seq))) = inbox.pending.peek().copied() {
                if at > now {
                    break;
                }
                inbox.pending.pop();
                if let Some(p) = inbox.packets.remove(&seq) {
                    inbox.ready.push_back(p);
                }
            }
            let mut n = 0;
            while n < bufs.len().min(meta.len()) {
                let Some(p) = inbox.ready.pop_front() else {
                    break;
                };
                let len = p.data.len().min(bufs[n].len());
                bufs[n][..len].copy_from_slice(&p.data[..len]);
                meta[n] = RecvMeta {
                    addr: p.from,
                    len,
                    stride: len,
                    ecn: None,
                    dst_ip: None,
                };
                n += 1;
            }
            s.stats.delivered += n as u64;
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
            inbox.waker = Some(cx.waker().clone());
            inbox.pending.peek().map(|Reverse((at, _))| *at)
        };
        if let Some(at) = next_due {
            let mut timer = self.timer.lock().unwrap_or_else(|p| p.into_inner());
            let sleep = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
            sleep.as_mut().reset(at);
            if sleep.as_mut().poll(cx).is_ready() {
                cx.waker().wake_by_ref();
            }
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// Small deterministic PRNG so runs are reproducible from a seed.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prng_is_uniformish_and_deterministic() {
        let mut a = SplitMix64(1);
        let mut b = SplitMix64(1);
        let xs: Vec<f64> = (0..10_000).map(|_| a.next_f64()).collect();
        assert!(xs.iter().all(|x| (0.0..1.0).contains(x)));
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!((mean - 0.5).abs() < 0.02);
        assert_eq!(xs[0], b.next_f64());
    }
}
