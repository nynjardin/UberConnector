//! UDP hole punching + local bridge to EasyTier.
//!
//! One shared "punch" socket talks to every remote peer. Once a path to a
//! peer is open, a per-peer local "bridge" socket relays datagrams between the
//! local EasyTier instance and that path:
//!
//!   EasyTier A <-> bridge A <-> punch A <~~ Internet ~~> punch B <-> bridge B <-> EasyTier B
//!
//! The peer with the lowest id is the "dialer": its EasyTier connects to its
//! bridge; the other side forwards to its EasyTier UDP listener.

use crate::discovery::{self, PeerRecord};
use crate::room::Room;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const MAGIC: &[u8; 3] = b"UCP";
const T_PROBE: u8 = 1;
const T_ACK: u8 = 2;
const T_DATA: u8 = 3;
/// Our presence record, encrypted with the salon key, sent to an address a
/// tracker gave us (see `tracker`). Followed by `I_HELLO` or `I_ANSWER`.
const T_INTRO: u8 = 4;
const I_HELLO: u8 = 1;
const I_ANSWER: u8 = 2;
/// Hellos to a tracker address go on this long (it may be stale)...
const INTRO_WINDOW: Duration = Duration::from_secs(30);
/// ...and the same address is not introduced again before this.
const INTRO_AGAIN: Duration = Duration::from_secs(600);
/// We answer the hellos of one address at most this often.
const ANSWER_EVERY: Duration = Duration::from_secs(2);
/// Aggressive probing duration after (re)discovering a peer.
const PUNCH_WINDOW: Duration = Duration::from_secs(45);
/// An open path silent for this long is dead (keepalives go every 5 s): the
/// peer restarted, possibly from a new address that must then be accepted.
pub const PATH_DEAD_AFTER: Duration = Duration::from_secs(12);

pub enum PunchEvent {
    /// A path to this peer is open; the dialer must connect EasyTier to `bridge`.
    Established { id: String, remote: SocketAddr, bridge: SocketAddr, dialer: bool },
    /// A peer we don't know yet probes us (a newcomer): read the DHT now.
    Unknown { id: String },
    /// A member's record, received directly from `from` after an
    /// introduction (tracker address).
    Record { rec: PeerRecord, from: SocketAddr },
}

/// A tracker address we say hello to.
struct Intro {
    since: Instant,
    /// A member answered from there: no more hellos.
    done: bool,
}

struct Link {
    candidates: Vec<SocketAddr>,
    established: Option<SocketAddr>,
    bridge: Arc<UdpSocket>,
    /// Where bridge traffic goes on our side (the local EasyTier socket).
    et_addr: Option<SocketAddr>,
    dialer: bool,
    probing_since: Instant,
    last_rx: Option<Instant>,
    /// Last address whose probes we ignored (logged once).
    ignored: Option<SocketAddr>,
    /// The link's `bridge_loop`, ended with the link.
    task: Option<tokio::task::AbortHandle>,
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

pub struct Puncher {
    sock: Arc<UdpSocket>,
    my_id: [u8; 8],
    my_id_hex: String,
    lan_ip: Ipv4Addr,
    /// Salon key and our record, once known (`set_intro`).
    intro_rec: Mutex<Option<(Arc<Room>, PeerRecord)>>,
    intros: Mutex<HashMap<SocketAddr, Intro>>,
    answered: Mutex<HashMap<SocketAddr, Instant>>,
    et_listener: SocketAddr,
    /// When false, LAN candidates are ignored (simulates remote players on one LAN).
    use_lan: bool,
    links: Mutex<HashMap<String, Link>>,
    by_addr: Mutex<HashMap<SocketAddr, String>>,
    events: mpsc::UnboundedSender<PunchEvent>,
    /// `recv_loop` and `probe_loop`.
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// LAN address of the default route interface (no packet is sent).
pub fn lan_ipv4() -> Ipv4Addr {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:53")?;
            s.local_addr()
        })
        .ok()
        .and_then(|a| match a.ip() {
            IpAddr::V4(ip) => Some(ip),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

impl Puncher {
    pub async fn bind() -> Result<UdpSocket> {
        Ok(UdpSocket::bind("0.0.0.0:0").await?)
    }

    /// Takes ownership of the socket used for STUN so the NAT mapping is reused.
    pub fn new(
        sock: UdpSocket,
        my_id: &str,
        lan_ip: Ipv4Addr,
        et_port: u16,
        use_lan: bool,
        events: mpsc::UnboundedSender<PunchEvent>,
    ) -> Arc<Self> {
        let mut id = [0u8; 8];
        hex::decode_to_slice(my_id, &mut id).ok();
        let p = Arc::new(Self {
            sock: Arc::new(sock),
            my_id: id,
            my_id_hex: my_id.to_string(),
            lan_ip,
            intro_rec: Mutex::new(None),
            intros: Default::default(),
            answered: Default::default(),
            et_listener: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), et_port),
            use_lan,
            links: Default::default(),
            by_addr: Default::default(),
            events,
            tasks: Default::default(),
        });
        let tasks = vec![
            tokio::spawn(p.clone().recv_loop()).abort_handle(),
            tokio::spawn(p.clone().probe_loop()).abort_handle(),
        ];
        *p.tasks.lock().unwrap() = tasks;
        p
    }

    /// Ends the session's punching: without it the tasks (and sockets) live
    /// on, and their keepalives make members believe our old path is alive.
    pub fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        self.links.lock().unwrap().clear();
        self.by_addr.lock().unwrap().clear();
        self.intros.lock().unwrap().clear();
    }

    /// What we send to tracker addresses: our record, readable by members only.
    pub fn set_intro(&self, room: Arc<Room>, rec: &PeerRecord) {
        *self.intro_rec.lock().unwrap() = Some((room, rec.clone()));
    }

    /// An address a tracker gave us: hellos for a while, until a member
    /// answers from there (it may also be stale, or someone else's).
    /// Returns whether hellos start now.
    pub fn introduce(&self, addr: SocketAddr) -> bool {
        // A member we already know (its open path, or its own address).
        if self.by_addr.lock().unwrap().contains_key(&addr) || self.links.lock().unwrap().values().any(|l| l.candidates.contains(&addr)) {
            return false;
        }
        let mut intros = self.intros.lock().unwrap();
        let start = intros.get(&addr).is_none_or(|i| i.since.elapsed() > INTRO_AGAIN);
        if start {
            intros.insert(addr, Intro { since: Instant::now(), done: false });
        }
        start
    }

    /// `T_INTRO` packet carrying our record, freshly dated.
    fn intro_packet(&self, kind: u8) -> Option<Vec<u8>> {
        let (room, mut rec) = self.intro_rec.lock().unwrap().clone()?;
        rec.ts = discovery::now_secs();
        let plain = serde_json::to_vec(&rec).ok()?;
        let mut pkt = vec![MAGIC[0], MAGIC[1], MAGIC[2], T_INTRO, kind];
        pkt.extend(room.encrypt(&plain));
        Some(pkt)
    }

    async fn on_intro(&self, kind: u8, body: &[u8], from: SocketAddr) {
        // Anyone can send this: only what decrypts with the salon key counts.
        let Some(room) = self.intro_rec.lock().unwrap().as_ref().map(|(r, _)| r.clone()) else { return };
        let Some(rec) = room.decrypt(body).ok().and_then(|p| serde_json::from_slice::<PeerRecord>(&p).ok()) else { return };
        if !discovery::is_other(&rec, &self.my_id_hex) {
            return;
        }
        if let Some(i) = self.intros.lock().unwrap().get_mut(&from) {
            i.done = true;
        }
        if kind == I_HELLO {
            let due = {
                let mut answered = self.answered.lock().unwrap();
                let due = answered.get(&from).is_none_or(|t| t.elapsed() > ANSWER_EVERY);
                if due {
                    answered.insert(from, Instant::now());
                }
                due
            };
            if let Some(pkt) = due.then(|| self.intro_packet(I_ANSWER)).flatten() {
                let _ = self.sock.send_to(&pkt, from).await;
            }
        }
        let _ = self.events.send(PunchEvent::Record { rec, from });
    }

    pub fn local_port(&self) -> u16 {
        self.sock.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn lan_addr(&self) -> SocketAddr {
        SocketAddr::new(self.lan_ip.into(), self.local_port())
    }

    /// Adds or refreshes a peer from its DHT record.
    pub async fn upsert_peer(self: &Arc<Self>, rec: &PeerRecord, my_id: &str) -> Result<()> {
        let lan = rec.punch_lan.filter(|_| self.use_lan);
        let candidates: Vec<SocketAddr> = rec.punch.iter().chain(lan.iter()).copied().collect();
        {
            let mut links = self.links.lock().unwrap();
            if let Some(link) = links.get_mut(&rec.id) {
                if link.candidates != candidates {
                    // New session of that peer: our open path may lead to
                    // its previous one. Punch again.
                    let dropped = link.established.filter(|a| !candidates.contains(a));
                    if let Some(old) = dropped {
                        link.established = None;
                        self.by_addr.lock().unwrap().remove(&old);
                    }
                    tracing::info!(
                        "perçage : {} ({}) change d'adresse : {candidates:?}{}",
                        rec.name,
                        short(&rec.id),
                        dropped.map(|old| format!(", chemin {old} abandonné, nouveau perçage")).unwrap_or_default()
                    );
                    link.candidates = candidates;
                    if link.established.is_none() {
                        link.probing_since = Instant::now();
                    }
                }
                return Ok(());
            }
        }
        let dialer = my_id < rec.id.as_str();
        tracing::info!("perçage : {} ({}) à {candidates:?}", rec.name, short(&rec.id));
        // Loopback only: on the LAN address any machine nearby could send to
        // it, and the dialer side takes the first sender for EasyTier.
        let bridge = Arc::new(UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await?);
        let link = Link {
            candidates,
            established: None,
            bridge: bridge.clone(),
            et_addr: if dialer { None } else { Some(self.et_listener) },
            dialer,
            probing_since: Instant::now(),
            last_rx: None,
            ignored: None,
            task: None,
        };
        self.links.lock().unwrap().insert(rec.id.clone(), link);
        let task = tokio::spawn(self.clone().bridge_loop(rec.id.clone(), bridge)).abort_handle();
        match self.links.lock().unwrap().get_mut(&rec.id) {
            Some(l) => l.task = Some(task),
            None => task.abort(),
        }
        Ok(())
    }

    pub fn remove_peer(&self, id: &str) {
        if let Some(link) = self.links.lock().unwrap().remove(id) {
            if let Some(a) = link.established {
                self.by_addr.lock().unwrap().remove(&a);
            }
        }
    }

    /// (remote addr, last packet age) of an open path, if any.
    pub fn path(&self, id: &str) -> Option<(SocketAddr, Duration)> {
        let links = self.links.lock().unwrap();
        let l = links.get(id)?;
        Some((l.established?, l.last_rx?.elapsed()))
    }

    fn probe(&self, t: u8) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..3].copy_from_slice(MAGIC);
        b[3] = t;
        b[4..].copy_from_slice(&self.my_id);
        b
    }

    async fn probe_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            let targets: Vec<SocketAddr> = {
                let links = self.links.lock().unwrap();
                // Every 30 s for 5 min, the peers still unreachable (diagnostic).
                if n % 60 == 0 {
                    let stuck = |l: &Link| l.established.is_none() && l.probing_since.elapsed() < Duration::from_secs(300);
                    for (id, l) in links.iter().filter(|(_, l)| stuck(l)) {
                        tracing::info!(
                            "perçage : toujours aucune réponse de {} depuis {} s (sondes vers {:?})",
                            short(id),
                            l.probing_since.elapsed().as_secs(),
                            l.candidates
                        );
                    }
                }
                links
                    .values()
                    .flat_map(|l| match l.established {
                        // Keepalive every 5 s keeps the NAT mapping open (and
                        // tells the peer the path is alive).
                        Some(a) if n % 10 == 0 => vec![a],
                        Some(_) => vec![],
                        None if l.probing_since.elapsed() < PUNCH_WINDOW => l.candidates.clone(),
                        // Retry slowly afterwards.
                        None if n % 10 == 0 => l.candidates.clone(),
                        None => vec![],
                    })
                    .collect()
            };
            let pkt = self.probe(T_PROBE);
            for t in targets {
                let _ = self.sock.send_to(&pkt, t).await;
            }
            // Tracker addresses: a hello every second while it may help.
            if n % 2 == 0 {
                let intros: Vec<SocketAddr> = {
                    let mut intros = self.intros.lock().unwrap();
                    intros.retain(|_, i| i.since.elapsed() < INTRO_AGAIN);
                    intros.iter().filter(|(_, i)| !i.done && i.since.elapsed() < INTRO_WINDOW).map(|(a, _)| *a).collect()
                };
                if let Some(hello) = (!intros.is_empty()).then(|| self.intro_packet(I_HELLO)).flatten() {
                    for a in intros {
                        let _ = self.sock.send_to(&hello, a).await;
                    }
                }
            }
        }
    }

    async fn recv_loop(self: Arc<Self>) {
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, from)) = self.sock.recv_from(&mut buf).await else {
                // Windows reports ICMP port unreachable as a recv error; ignore.
                continue;
            };
            let pkt = &buf[..n];
            if n < 4 || &pkt[..3] != MAGIC {
                continue;
            }
            match pkt[3] {
                T_PROBE | T_ACK if n >= 12 => {
                    let id = hex::encode(&pkt[4..12]);
                    // Acknowledge only a path we use: otherwise the peer
                    // would believe in a path we ignore.
                    if self.on_reachable(&id, from) && pkt[3] == T_PROBE {
                        let _ = self.sock.send_to(&self.probe(T_ACK), from).await;
                    }
                }
                T_INTRO if n > 5 => self.on_intro(pkt[4], &pkt[5..], from).await,
                T_DATA => {
                    let Some(id) = self.by_addr.lock().unwrap().get(&from).cloned() else { continue };
                    let target = {
                        let mut links = self.links.lock().unwrap();
                        let Some(l) = links.get_mut(&id) else { continue };
                        l.last_rx = Some(Instant::now());
                        l.et_addr.map(|a| (l.bridge.clone(), a))
                    };
                    if let Some((bridge, a)) = target {
                        let _ = bridge.send_to(&pkt[4..], a).await;
                    }
                }
                _ => {}
            }
        }
    }

    /// Returns whether `from` is (now) our path to that peer.
    fn on_reachable(&self, id: &str, from: SocketAddr) -> bool {
        let mut links = self.links.lock().unwrap();
        let Some(l) = links.get_mut(id) else {
            let _ = self.events.send(PunchEvent::Unknown { id: id.to_string() });
            return false;
        };
        if l.established == Some(from) {
            l.last_rx = Some(Instant::now());
            return true;
        }
        // First path wins; prefer a LAN path if it shows up later, and
        // replace a dead one (the peer restarted from another port).
        // Only packets on the established path prove it alive: probes from
        // the peer's new address must not keep its dead path "fresh".
        let is_lan = l.candidates.len() > 1 && Some(&from) == l.candidates.last();
        let dead = l.last_rx.is_none_or(|t| t.elapsed() > PATH_DEAD_AFTER);
        if let (Some(cur), false, false) = (l.established, is_lan, dead) {
            if l.ignored != Some(from) {
                l.ignored = Some(from);
                tracing::info!("perçage : sondes de {id} depuis {from} ignorées, le chemin {cur} est encore vivant");
            }
            return false;
        }
        if let (Some(cur), true) = (l.established, dead) {
            tracing::info!("perçage : chemin {cur} vers {id} silencieux, remplacé par {from}");
        }
        l.last_rx = Some(Instant::now());
        let mut by_addr = self.by_addr.lock().unwrap();
        if let Some(old) = l.established.replace(from) {
            by_addr.remove(&old);
        }
        by_addr.insert(from, id.to_string());
        let bridge = l.bridge.local_addr().unwrap();
        let _ = self.events.send(PunchEvent::Established {
            id: id.to_string(),
            remote: from,
            bridge,
            dialer: l.dialer,
        });
        true
    }

    /// Local EasyTier -> bridge -> punch socket -> remote peer.
    async fn bridge_loop(self: Arc<Self>, id: String, bridge: Arc<UdpSocket>) {
        let mut buf = vec![0u8; 65536];
        buf[..3].copy_from_slice(MAGIC);
        buf[3] = T_DATA;
        loop {
            let Ok((n, from)) = bridge.recv_from(&mut buf[4..]).await else {
                if !self.links.lock().unwrap().contains_key(&id) {
                    return;
                }
                continue;
            };
            let remote = {
                let mut links = self.links.lock().unwrap();
                let Some(l) = links.get_mut(&id) else { return };
                if l.dialer {
                    l.et_addr = Some(from);
                }
                l.established
            };
            if let Some(r) = remote {
                let _ = self.sock.send_to(&buf[..4 + n], r).await;
            }
        }
    }
}

/// Minimal STUN binding request (RFC 5389) on `sock`; returns the public mapping.
pub async fn stun(sock: &UdpSocket, servers: &[&str]) -> Result<SocketAddr> {
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    req[4..8].copy_from_slice(&0x2112A442u32.to_be_bytes());
    let txid: [u8; 12] = rand::random();
    req[8..20].copy_from_slice(&txid);
    for server in servers {
        let Ok(mut addrs) = tokio::net::lookup_host(server).await else { continue };
        let Some(addr) = addrs.find(|a| a.is_ipv4()) else { continue };
        for _ in 0..3 {
            sock.send_to(&req, addr).await?;
            let mut buf = [0u8; 512];
            let res = tokio::time::timeout(Duration::from_millis(800), sock.recv_from(&mut buf)).await;
            if let Ok(Ok((n, _))) = res {
                if let Some(a) = parse_stun(&buf[..n], &txid) {
                    return Ok(a);
                }
            }
        }
    }
    None.context("aucun serveur STUN n'a répondu")
}

fn parse_stun(b: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if b.len() < 20 || b[0..2] != [0x01, 0x01] || &b[8..20] != txid {
        return None;
    }
    let mut i = 20;
    while i + 4 <= b.len() {
        let t = u16::from_be_bytes([b[i], b[i + 1]]);
        let l = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let v = b.get(i + 4..i + 4 + l)?;
        // XOR-MAPPED-ADDRESS (0x0020) or MAPPED-ADDRESS (0x0001), IPv4 only.
        if (t == 0x0020 || t == 0x0001) && l >= 8 && v[1] == 0x01 {
            let mut port = u16::from_be_bytes([v[2], v[3]]);
            let mut ip = [v[4], v[5], v[6], v[7]];
            if t == 0x0020 {
                port ^= 0x2112;
                for (k, m) in ip.iter_mut().zip([0x21, 0x12, 0xA4, 0x42]) {
                    *k ^= m;
                }
            }
            return Some(SocketAddr::new(Ipv4Addr::from(ip).into(), port));
        }
        i += 4 + l.div_ceil(4) * 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, name: &str) -> PeerRecord {
        PeerRecord {
            id: id.into(),
            name: name.into(),
            avatar: String::new(),
            vip: Ipv4Addr::new(10, 1, 0, 2),
            ts: 0,
            punch: None,
            punch_lan: None,
            direct: vec![],
            chat_port: 0,
        }
    }

    async fn puncher(id: &str, room: &Arc<Room>) -> (Arc<Puncher>, mpsc::UnboundedReceiver<PunchEvent>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let p = Puncher::new(sock, id, Ipv4Addr::LOCALHOST, 9, true, tx);
        p.set_intro(room.clone(), &record(id, id));
        (p, rx)
    }

    async fn next_record(rx: &mut mpsc::UnboundedReceiver<PunchEvent>) -> Option<PeerRecord> {
        let wait = async {
            loop {
                if let Some(PunchEvent::Record { rec, .. }) = rx.recv().await {
                    return rec;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(3), wait).await.ok()
    }

    /// A tracker address gets our record; the member there answers with its own.
    #[tokio::test]
    async fn introduction_exchanges_records() {
        let room = Arc::new(Room::new("salon", &[7; 32]));
        let (a, mut a_rx) = puncher("aaaaaaaaaaaaaaaa", &room).await;
        let (b, mut b_rx) = puncher("bbbbbbbbbbbbbbbb", &room).await;
        a.introduce(b.lan_addr());
        assert_eq!(next_record(&mut b_rx).await.map(|r| r.name), Some("aaaaaaaaaaaaaaaa".into()));
        assert_eq!(next_record(&mut a_rx).await.map(|r| r.name), Some("bbbbbbbbbbbbbbbb".into()));
        a.stop();
        b.stop();
    }

    /// Someone outside the salon is neither heard nor answered.
    #[tokio::test]
    async fn introduction_needs_the_salon_key() {
        let (a, mut a_rx) = puncher("aaaaaaaaaaaaaaaa", &Arc::new(Room::new("salon", &[7; 32]))).await;
        let (b, mut b_rx) = puncher("bbbbbbbbbbbbbbbb", &Arc::new(Room::new("salon", &[8; 32]))).await;
        a.introduce(b.lan_addr());
        assert!(next_record(&mut b_rx).await.is_none());
        assert!(next_record(&mut a_rx).await.is_none());
        a.stop();
        b.stop();
    }
}
