//! Fastest rendezvous: public UDP BitTorrent trackers (BEP 15).
//!
//! The idea comes from SmartSteamEmu's online mode (see ROADMAP). Every
//! member announces the salon's swarm (`Room::tracker_hash`) with the public
//! port of its hole punching socket; a tracker answers in one round trip
//! with the addresses of the members already there (0.2 to 0.6 s measured,
//! against 6 to 14 s for the DHT). Each address then gets an encrypted hello
//! carrying our presence record (see `punch`), so it enters the usual path.
//!
//! A tracker only stores addresses: it sees that some swarm has members,
//! never the salon nor anything about it. Trackers never push, so the first
//! member there only learns about the next one at its own next announce:
//! we announce every few seconds during the first minute, then rarely (the
//! newcomer finds us at once and says hello itself).

use anyhow::{Context, Result, bail};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Measured from New Caledonia on 2026-11-14 (two ISPs): all answered in
/// under 3 s. Too slow and left out: tracker.theoks.net, tracker.dler.org.
pub const TRACKERS: &[&str] = &[
    "open.demonii.com:1337",
    "explodie.org:6969",
    "tracker-udp.gbitt.info:80",
    "open.stealth.si:80",
    "tracker.opentrackr.org:1337",
    "tracker.qu.ax:6969",
    "tracker.torrent.eu.org:451",
    "exodus.desync.com:6969",
    "tracker.internetwarriors.net:1337",
];
/// BEP 15 magic for the connect request.
const PROTOCOL_ID: u64 = 0x0417_2710_1980;
/// BEP 15 says 15 s; a tracker that slow is useless here, others answer.
const TIMEOUT: Duration = Duration::from_secs(2);
const TRIES: usize = 3;
const FAST_EVERY: Duration = Duration::from_secs(3);
const FAST_PHASE: Duration = Duration::from_secs(60);
const SLOW_EVERY: Duration = Duration::from_secs(120);
/// A failing tracker is tried again after this delay.
const RETRY_FAILED: Duration = Duration::from_secs(60);

const EV_NONE: u32 = 0;
const EV_STARTED: u32 = 2;
const EV_STOPPED: u32 = 3;

pub enum TrEvent {
    /// A tracker accepted our announce (first one: worth a milestone).
    Ready(String),
    /// An address announced in the salon's swarm (maybe ours, maybe stale).
    Peer(SocketAddr, String),
}

struct Shared {
    hash: [u8; 20],
    peer_id: [u8; 20],
    /// Port the others must use (opened on the router, or the STUN one).
    port: u16,
    /// Our own announced address, left out of the results.
    me: Option<SocketAddrV4>,
    events: mpsc::UnboundedSender<TrEvent>,
}

pub struct Trackers {
    shared: Arc<Shared>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Trackers {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Trackers {
    /// Announces `hash` with `public` (the address others should use) on
    /// every tracker, now and periodically.
    pub fn start(hash: [u8; 20], public: SocketAddr, events: mpsc::UnboundedSender<TrEvent>) -> Self {
        let mut peer_id = [0u8; 20];
        peer_id[..8].copy_from_slice(b"-UC0001-");
        peer_id[8..].copy_from_slice(&rand::random::<[u8; 12]>());
        let me = match public {
            SocketAddr::V4(a) => Some(a),
            SocketAddr::V6(_) => None,
        };
        let shared = Arc::new(Shared { hash, peer_id, port: public.port(), me, events });
        let started = Instant::now();
        let tasks = TRACKERS.iter().map(|host| tokio::spawn(tracker_loop(shared.clone(), host, started))).collect();
        Self { shared, tasks }
    }

    /// Leaves the swarm (best effort, 2 s at most): members arriving later
    /// do not get our dead address.
    pub async fn stop(self) {
        for t in &self.tasks {
            t.abort();
        }
        let s = self.shared.clone();
        let stops = TRACKERS.iter().map(|host| {
            let s = s.clone();
            async move {
                if let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await {
                    let _ = tokio::time::timeout(Duration::from_secs(2), announce(&sock, host, &s, EV_STOPPED)).await;
                }
            }
        });
        futures::future::join_all(stops).await;
    }
}

async fn tracker_loop(s: Arc<Shared>, host: &'static str, started: Instant) {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else { return };
    let mut event = EV_STARTED;
    // Failures are logged once in a row (a dead tracker is retried quietly).
    let mut quiet = false;
    loop {
        let t = Instant::now();
        let wait = match announce(&sock, host, &s, event).await {
            Ok(peers) => {
                if event == EV_STARTED {
                    tracing::info!("tracker {host} : annoncé en {} ms, {} adresse(s)", t.elapsed().as_millis(), peers.len());
                    let _ = s.events.send(TrEvent::Ready(host.to_string()));
                }
                quiet = false;
                event = EV_NONE;
                for p in peers {
                    if usable(p, s.me) {
                        let _ = s.events.send(TrEvent::Peer(SocketAddr::V4(p), host.to_string()));
                    }
                }
                if started.elapsed() < FAST_PHASE { FAST_EVERY } else { SLOW_EVERY }
            }
            Err(e) => {
                if !quiet {
                    tracing::info!("tracker {host} indisponible ({e:#}), nouvel essai toutes les {} s", RETRY_FAILED.as_secs());
                    quiet = true;
                }
                RETRY_FAILED
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// Some trackers pad their answers with junk such as 0.0.0.0:65535.
fn usable(p: SocketAddrV4, me: Option<SocketAddrV4>) -> bool {
    let ip = p.ip();
    Some(p) != me && p.port() != 0 && !ip.is_unspecified() && !ip.is_loopback() && !ip.is_broadcast() && !ip.is_multicast()
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Sends `pkt` until a reply with this transaction id and `action` arrives.
async fn request(sock: &UdpSocket, to: SocketAddr, pkt: &[u8], tx: u32, action: u32) -> Result<Vec<u8>> {
    let mut buf = [0u8; 2048];
    for _ in 0..TRIES {
        sock.send_to(pkt, to).await?;
        let end = Instant::now() + TIMEOUT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let Ok(r) = tokio::time::timeout(left, sock.recv_from(&mut buf)).await else { break };
            // Windows reports an ICMP "port unreachable" as a recv error.
            let (n, from) = r.context("port fermé")?;
            let b = &buf[..n];
            if from != to || n < 8 || be32(&b[4..8]) != tx {
                continue;
            }
            match be32(&b[0..4]) {
                a if a == action => return Ok(b.to_vec()),
                3 => bail!("refus : {}", String::from_utf8_lossy(&b[8..])),
                _ => continue,
            }
        }
    }
    bail!("pas de réponse")
}

/// One BEP 15 connect + announce; returns the addresses in the swarm.
async fn announce(sock: &UdpSocket, host: &str, s: &Shared, event: u32) -> Result<Vec<SocketAddrV4>> {
    let to = tokio::net::lookup_host(host).await.context("DNS")?.find(|a| a.is_ipv4()).context("pas d'adresse IPv4")?;
    let tx: u32 = rand::random();
    let mut c = Vec::with_capacity(16);
    c.extend(PROTOCOL_ID.to_be_bytes());
    c.extend(0u32.to_be_bytes());
    c.extend(tx.to_be_bytes());
    let r = request(sock, to, &c, tx, 0).await.context("connexion")?;
    if r.len() < 16 {
        bail!("réponse de connexion trop courte");
    }
    let tx: u32 = rand::random();
    let mut a = Vec::with_capacity(98);
    a.extend(&r[8..16]); // connection id
    a.extend(1u32.to_be_bytes()); // action: announce
    a.extend(tx.to_be_bytes());
    a.extend(s.hash);
    a.extend(s.peer_id);
    a.extend(0u64.to_be_bytes()); // downloaded
    a.extend(1u64.to_be_bytes()); // left: a leecher gets every peer, a seeder may not
    a.extend(0u64.to_be_bytes()); // uploaded
    a.extend(event.to_be_bytes());
    a.extend(0u32.to_be_bytes()); // ip: the sender's
    a.extend(rand::random::<u32>().to_be_bytes()); // key
    a.extend((-1i32).to_be_bytes()); // num_want: default
    a.extend(s.port.to_be_bytes());
    let r = request(sock, to, &a, tx, 1).await.context("annonce")?;
    if r.len() < 20 {
        bail!("réponse d'annonce trop courte");
    }
    Ok(parse_peers(&r[20..]))
}

fn parse_peers(b: &[u8]) -> Vec<SocketAddrV4> {
    b.chunks_exact(6)
        .map(|p| SocketAddrV4::new(Ipv4Addr::new(p[0], p[1], p[2], p[3]), u16::from_be_bytes([p[4], p[5]])))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_are_parsed_and_junk_dropped() {
        let me = SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 5000);
        let raw = [1, 2, 3, 4, 0x13, 0x88, 0, 0, 0, 0, 0xff, 0xff, 5, 6, 7, 8, 0x1f, 0x90, 9];
        let peers = parse_peers(&raw);
        assert_eq!(peers.len(), 3);
        let kept: Vec<_> = peers.into_iter().filter(|p| usable(*p, Some(me))).collect();
        assert_eq!(kept, vec![SocketAddrV4::new(Ipv4Addr::new(5, 6, 7, 8), 8080)]);
    }

    #[test]
    fn swarm_depends_on_the_salon_key() {
        use crate::room::Room;
        let a = Room::new("salon", &[7; 32]).tracker_hash();
        assert_eq!(a, Room::new("salon", &[7; 32]).tracker_hash());
        assert_ne!(a, Room::new("salon", &[8; 32]).tracker_hash());
        assert_ne!(a, Room::new("autre", &[7; 32]).tracker_hash());
    }
}
