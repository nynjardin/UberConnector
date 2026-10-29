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

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Probes per kind, spaced out (UDP may drop one).
const ROUNDS: u32 = 3;
const ROUND_GAP: Duration = Duration::from_millis(400);
/// Answers are awaited this long after the last probe.
const WAIT: Duration = Duration::from_secs(3);

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
        });
        tokio::spawn(net.clone().recv_loop(net.sock.clone()));
        Ok(net)
    }

    async fn bound_socket(self: &Arc<Self>) -> Option<Arc<UdpSocket>> {
        let mut bound = self.bound.lock().await;
        if bound.is_none() {
            let s = UdpSocket::bind((self.my_vip, 0)).await.ok().filter(|s| s.set_broadcast(true).is_ok())?;
            let s = Arc::new(s);
            // Answers to its broadcasts come back to it.
            tokio::spawn(self.clone().recv_loop(s.clone()));
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
}
