//! "Tester le réseau de jeu": checks what a LAN game needs on the virtual
//! adapter, without the game. Every member in game mode listens on a UDP port
//! of the salon; the test sends, like a game would:
//! - unicast to each player's virtual IP (games joined by IP);
//! - a broadcast on the salon subnet (10.X.255.255);
//! - a broadcast to 255.255.255.255 (lobby announcements of most games),
//!   twice: from our virtual IP (games where the player picks the IP), and
//!   from any address (Windows then picks the adapter: the physical one,
//!   relayed by EasyTier, unless another VPN's adapter wins).
//! Each receiver answers by unicast to the packet's source address, as a game
//! would.
//!
//! Between tests, a direct probe goes to each player every second (see
//! `PingStats`): the ping as the game sees it, through the virtual adapter,
//! unsmoothed, to spot an unstable link that loses nothing.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Probes per kind, spaced out (UDP may drop one).
const ROUNDS: u32 = 3;
const ROUND_GAP: Duration = Duration::from_millis(400);
/// Answers are awaited this long after the last probe.
const WAIT: Duration = Duration::from_secs(3);
/// Continuous ping: one probe per player per second, kept 5 minutes.
const PING_EVERY: Duration = Duration::from_secs(1);
const PING_KEEP: usize = 300;
/// A probe unanswered after this long counts as lost.
const PING_LOST_MS: u32 = 2000;
/// Measured from this long after a player's first answer.
const PING_WARMUP_MS: u32 = 5000;
/// Samples shown as a sparkline (the last minute).
const PING_RECENT: usize = 60;
/// A spike: this much above the median.
pub const SPIKE_MS: u32 = 20;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Direct,
    Subnet,
    /// 255.255.255.255 from an unbound socket.
    Global,
    /// 255.255.255.255 from a socket bound to our virtual IP.
    GlobalBound,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    /// "uc-gt": not some other program's packet.
    uc: String,
    salon: String,
    kind: Kind,
    id: String,
    name: String,
    /// Sender's virtual IP (informative: answers go to the packet's source).
    vip: Ipv4Addr,
    nonce: u64,
    reply: bool,
    /// Milliseconds since the test started when the probe left; echoed back.
    #[serde(default)]
    t: u32,
}

/// One player's answers.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PlayerResult {
    pub id: String,
    pub name: String,
    /// Round trip of the fastest direct answer.
    pub direct_ms: Option<u32>,
    pub subnet: bool,
    /// Broadcast from any address (Windows picks the adapter).
    pub global: bool,
    /// Broadcast from our virtual IP.
    #[serde(default)]
    pub global_bound: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GameTest {
    pub running: bool,
    /// Why the test could not run.
    #[serde(default)]
    pub error: Option<String>,
    /// Players who answered or were probed, by name.
    pub results: Vec<PlayerResult>,
    /// Adapters that may take broadcasts sent from any address (see
    /// `system::broadcast_rivals`).
    #[serde(default)]
    pub rivals: Vec<String>,
    /// Announcements relayed so far by `bcast` (None: relay not running).
    #[serde(default)]
    pub relayed: Option<u64>,
}

/// Ping measured by the continuous probe over the last 5 minutes.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PingStats {
    /// Probes sent (and settled), lost among them.
    pub sent: u32,
    pub lost: u32,
    pub min: u32,
    pub median: u32,
    pub avg: u32,
    pub p95: u32,
    pub max: u32,
    /// Mean change between two consecutive answers (ms).
    pub jitter: f32,
    /// Answers at least `SPIKE_MS` above the median.
    pub spikes: u32,
    /// Last minute, oldest first (None: lost).
    pub recent: Vec<Option<u32>>,
}

impl PingStats {
    fn from_samples(samples: &[Option<u32>]) -> Option<Self> {
        let mut ok: Vec<u32> = samples.iter().flatten().copied().collect();
        if ok.is_empty() {
            let n = samples.len() as u32;
            return (n > 0).then(|| Self { sent: n, lost: n, recent: samples[samples.len().saturating_sub(PING_RECENT)..].to_vec(), ..Default::default() });
        }
        let jitter = {
            let d: Vec<u32> = ok.windows(2).map(|w| w[0].abs_diff(w[1])).collect();
            if d.is_empty() { 0.0 } else { d.iter().sum::<u32>() as f32 / d.len() as f32 }
        };
        ok.sort_unstable();
        let at = |q: f32| ok[((ok.len() - 1) as f32 * q).round() as usize];
        let median = at(0.5);
        Some(Self {
            sent: samples.len() as u32,
            lost: (samples.len() - ok.len()) as u32,
            min: ok[0],
            median,
            avg: (ok.iter().map(|&v| v as u64).sum::<u64>() / ok.len() as u64) as u32,
            p95: at(0.95),
            max: ok[ok.len() - 1],
            jitter,
            spikes: ok.iter().filter(|&&v| v >= median + SPIKE_MS).count() as u32,
            recent: samples[samples.len().saturating_sub(PING_RECENT)..].to_vec(),
        })
    }
}

/// Continuous ping state: probes are tagged with `nonce`, and `t` (ms since
/// `started`) identifies each round.
struct Monitor {
    nonce: u64,
    started: Instant,
    targets: Vec<(String, Ipv4Addr)>,
    /// Player id -> (probe time, round trip), oldest first.
    hist: HashMap<String, VecDeque<(u32, Option<u32>)>>,
    /// Player id -> time of its first answer (see `ping_stats`).
    answering: HashMap<String, u32>,
}

struct Run {
    nonce: u64,
    started: Instant,
    results: BTreeMap<String, PlayerResult>,
    running: bool,
}

pub struct GameNet {
    sock: Arc<UdpSocket>,
    port: u16,
    salon_id: String,
    my_id: String,
    my_vip: Ipv4Addr,
    subnet: u8,
    name: Mutex<String>,
    run: Mutex<Option<Run>>,
    /// Bound to our virtual IP (a game where the player picks the IP).
    /// Created at the first test: the address only exists once EasyTier
    /// has set up the adapter, seconds after the session starts.
    bound: tokio::sync::Mutex<Option<Arc<UdpSocket>>>,
    rivals: Mutex<Vec<String>>,
    mon: Mutex<Monitor>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

/// UDP port of the test, shared by the salon's members (not the chat's).
pub fn port_for(salon_id: &str) -> u16 {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("uc-game-test/{salon_id}").as_bytes());
    let p = 20000 + u16::from_be_bytes([h[0], h[1]]) % 40000;
    if p == crate::chat::port_for(salon_id) { p + 1 } else { p }
}

impl GameNet {
    /// Listens on `bind` (0.0.0.0 to get broadcasts) at the salon's port.
    pub async fn start(bind: Ipv4Addr, port: u16, salon_id: &str, my_id: &str, my_vip: Ipv4Addr) -> Result<Arc<Self>> {
        let sock = UdpSocket::bind((bind, port)).await?;
        sock.set_broadcast(true)?;
        let net = Arc::new(Self {
            sock: Arc::new(sock),
            port,
            salon_id: salon_id.to_string(),
            my_id: my_id.to_string(),
            my_vip,
            subnet: my_vip.octets()[1],
            name: Mutex::new(String::new()),
            run: Mutex::new(None),
            bound: Default::default(),
            rivals: Mutex::new(vec![]),
            mon: Mutex::new(Monitor { nonce: rand::random(), started: Instant::now(), targets: vec![], hist: HashMap::new(), answering: HashMap::new() }),
            tasks: Mutex::new(vec![]),
        });
        net.spawn(net.clone().recv_loop(net.sock.clone()));
        net.spawn(net.clone().ping_loop());
        Ok(net)
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.tasks.lock().unwrap().push(tokio::spawn(task).abort_handle());
    }

    /// Ends the background tasks (they hold the sockets, hence the port).
    pub fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }

    /// Players to ping continuously (id, virtual IP).
    pub fn set_ping_targets(&self, targets: Vec<(String, Ipv4Addr)>) {
        let mut m = self.mon.lock().unwrap();
        m.hist.retain(|id, _| targets.iter().any(|(t, _)| t == id));
        m.answering.retain(|id, _| targets.iter().any(|(t, _)| t == id));
        m.targets = targets;
    }

    /// Ping per player id over the last 5 minutes (settled probes only),
    /// from `PING_WARMUP_MS` after the player first answered: probes sent
    /// while the link was being set up would only measure the setup.
    pub fn ping_stats(&self) -> HashMap<String, PingStats> {
        let m = self.mon.lock().unwrap();
        let now = m.started.elapsed().as_millis() as u32;
        m.hist
            .iter()
            .filter_map(|(id, h)| {
                let from = m.answering.get(id)? + PING_WARMUP_MS;
                let settled: Vec<Option<u32>> = h
                    .iter()
                    .filter(|(t, rtt)| *t >= from && (rtt.is_some() || now.saturating_sub(*t) >= PING_LOST_MS))
                    .map(|(_, rtt)| *rtt)
                    .collect();
                Some((id.clone(), PingStats::from_samples(&settled)?))
            })
            .collect()
    }

    async fn ping_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(PING_EVERY);
        loop {
            tick.tick().await;
            let (nonce, t, targets) = {
                let mut m = self.mon.lock().unwrap();
                let t = m.started.elapsed().as_millis() as u32;
                let targets = m.targets.clone();
                for (id, _) in &targets {
                    let h = m.hist.entry(id.clone()).or_default();
                    h.push_back((t, None));
                    if h.len() > PING_KEEP {
                        h.pop_front();
                    }
                }
                (m.nonce, t, targets)
            };
            // A "direct" probe: members on older versions answer it too.
            let pkt = self.wire(Kind::Direct, nonce, false, t);
            for (_, vip) in &targets {
                let _ = self.sock.send_to(&pkt, (*vip, self.port)).await;
            }
        }
    }

    async fn bound_socket(self: &Arc<Self>) -> Option<Arc<UdpSocket>> {
        let mut bound = self.bound.lock().await;
        if bound.is_none() {
            let s = UdpSocket::bind((self.my_vip, 0)).await.ok().filter(|s| s.set_broadcast(true).is_ok())?;
            let s = Arc::new(s);
            // Answers to its broadcasts come back to it.
            self.spawn(self.clone().recv_loop(s.clone()));
            *bound = Some(s);
        }
        bound.clone()
    }

    pub fn set_rivals(&self, rivals: Vec<String>) {
        *self.rivals.lock().unwrap() = rivals;
    }

    pub fn set_name(&self, name: &str) {
        *self.name.lock().unwrap() = name.to_string();
    }

    pub fn snapshot(&self) -> Option<GameTest> {
        self.run.lock().unwrap().as_ref().map(|r| GameTest {
            running: r.running,
            error: None,
            rivals: self.rivals.lock().unwrap().clone(),
            relayed: None,
            results: r.results.values().cloned().collect(),
        })
    }

    fn wire(&self, kind: Kind, nonce: u64, reply: bool, t: u32) -> Vec<u8> {
        serde_json::to_vec(&Wire {
            uc: "uc-gt".into(),
            salon: self.salon_id.clone(),
            kind,
            id: self.my_id.clone(),
            name: self.name.lock().unwrap().clone(),
            vip: self.my_vip,
            nonce,
            reply,
            t,
        })
        .unwrap()
    }

    /// Runs a test against `players` (id, name, virtual IP); results show
    /// up in `snapshot` as answers arrive.
    pub fn run(self: &Arc<Self>, players: Vec<(String, String, Ipv4Addr)>) {
        let nonce = rand::random::<u64>();
        let started = Instant::now();
        let results = players
            .iter()
            .map(|(id, name, _)| (id.clone(), PlayerResult { id: id.clone(), name: name.clone(), ..Default::default() }))
            .collect();
        *self.run.lock().unwrap() = Some(Run { nonce, started, results, running: true });
        let me = self.clone();
        tokio::spawn(async move {
            let subnet = SocketAddr::from((Ipv4Addr::new(10, me.subnet, 255, 255), me.port));
            let global = SocketAddr::from((Ipv4Addr::BROADCAST, me.port));
            let bound = me.bound_socket().await;
            for _ in 0..ROUNDS {
                let t = started.elapsed().as_millis() as u32;
                for (_, _, vip) in &players {
                    let _ = me.sock.send_to(&me.wire(Kind::Direct, nonce, false, t), (*vip, me.port)).await;
                }
                let _ = me.sock.send_to(&me.wire(Kind::Subnet, nonce, false, t), subnet).await;
                let _ = me.sock.send_to(&me.wire(Kind::Global, nonce, false, t), global).await;
                if let Some(b) = &bound {
                    let _ = b.send_to(&me.wire(Kind::GlobalBound, nonce, false, t), global).await;
                }
                tokio::time::sleep(ROUND_GAP).await;
            }
            tokio::time::sleep(WAIT).await;
            if let Some(r) = me.run.lock().unwrap().as_mut().filter(|r| r.nonce == nonce) {
                r.running = false;
            }
        });
    }

    async fn recv_loop(self: Arc<Self>, sock: Arc<UdpSocket>) {
        let mut buf = vec![0u8; 2048];
        loop {
            // Windows reports ICMP "port unreachable" as a receive error.
            let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
            let Ok(w) = serde_json::from_slice::<Wire>(&buf[..n]) else { continue };
            if w.uc != "uc-gt" || w.salon != self.salon_id || w.id == self.my_id {
                continue;
            }
            if !w.reply {
                // To the packet's source, like a game would: a relayed
                // announcement must come from an address we can answer.
                let answer = self.wire(w.kind, w.nonce, true, w.t);
                let _ = sock.send_to(&answer, from).await;
                continue;
            }
            {
                let mut m = self.mon.lock().unwrap();
                if w.nonce == m.nonce {
                    let now = m.started.elapsed().as_millis() as u32;
                    let rtt = now.saturating_sub(w.t);
                    // Too late: already counted as lost.
                    if rtt < PING_LOST_MS && m.hist.contains_key(&w.id) {
                        m.answering.entry(w.id.clone()).or_insert(now);
                        if let Some(s) = m.hist.get_mut(&w.id).and_then(|h| h.iter_mut().find(|(t, _)| *t == w.t)) {
                            s.1.get_or_insert(rtt);
                        }
                    }
                    continue;
                }
            }
            let mut run = self.run.lock().unwrap();
            let Some(r) = run.as_mut().filter(|r| r.nonce == w.nonce) else { continue };
            let ms = (r.started.elapsed().as_millis() as u32).saturating_sub(w.t);
            let p = r.results.entry(w.id.clone()).or_insert_with(|| PlayerResult { id: w.id.clone(), ..Default::default() });
            // Peers name themselves once they have run a test.
            if !w.name.is_empty() {
                p.name = w.name;
            }
            match w.kind {
                Kind::Direct => p.direct_ms = Some(p.direct_ms.map_or(ms, |m| m.min(ms))),
                Kind::Subnet => p.subnet = true,
                Kind::Global => p.global = true,
                Kind::GlobalBound => p.global_bound = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two members on loopback addresses: direct probes are answered.
    #[tokio::test]
    async fn direct_probe_is_answered() {
        let port = 47_311;
        let a_ip = Ipv4Addr::new(127, 0, 0, 1);
        let b_ip = Ipv4Addr::new(127, 0, 0, 2);
        let a = GameNet::start(a_ip, port, "salon", "aaaa", a_ip).await.unwrap();
        let b = GameNet::start(b_ip, port, "salon", "bbbb", b_ip).await.unwrap();
        b.set_name("Bob");
        a.run(vec![("bbbb".into(), "B".into(), b_ip)]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let t = a.snapshot().unwrap();
        assert!(t.running);
        let r = &t.results[0];
        assert_eq!(r.name, "Bob");
        assert!(r.direct_ms.is_some());
        // Another salon's member does not answer.
        let c = GameNet::start(Ipv4Addr::new(127, 0, 0, 3), port, "autre", "cccc", Ipv4Addr::new(127, 0, 0, 3)).await.unwrap();
        a.run(vec![("cccc".into(), "C".into(), Ipv4Addr::new(127, 0, 0, 3))]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(a.snapshot().unwrap().results[0].direct_ms.is_none());
        drop(c);
    }

    /// The continuous probe measures each target's ping, once warmed up.
    #[tokio::test]
    async fn continuous_ping() {
        let port = 47_312;
        let a_ip = Ipv4Addr::new(127, 0, 0, 1);
        let b_ip = Ipv4Addr::new(127, 0, 0, 2);
        let a = GameNet::start(a_ip, port, "salon", "aaaa", a_ip).await.unwrap();
        let b = GameNet::start(b_ip, port, "salon", "bbbb", b_ip).await.unwrap();
        a.set_ping_targets(vec![("bbbb".into(), b_ip)]);
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert!(a.ping_stats().is_empty());
        tokio::time::sleep(Duration::from_millis(5000)).await;
        let s = a.ping_stats().remove("bbbb").unwrap();
        assert!(s.sent >= 1 && s.lost == 0, "{s:?}");
        assert_eq!(s.recent.len() as u32, s.sent);
        b.stop();
        a.stop();
    }

    #[test]
    fn stats_from_samples() {
        let s = PingStats::from_samples(&[Some(10), Some(12), None, Some(50), Some(10)]).unwrap();
        assert_eq!((s.sent, s.lost, s.min, s.max, s.median, s.spikes), (5, 1, 10, 50, 12, 1));
        assert!((s.jitter - (2.0 + 38.0 + 40.0) / 3.0).abs() < 0.01);
        assert!(PingStats::from_samples(&[]).is_none());
    }
}
