//! Prototype: are public UDP BitTorrent trackers (BEP 15) a faster rendezvous
//! than the DHT? This is how SmartSteamEmu's online mode finds players:
//! hash(key) announced to UDP trackers with the STUN port, then plain UDP
//! packets to every address returned (hole punching).
//!
//! Solo (one PC): cargo run -p uc-core --example tracker_probe
//!   For each tracker, two fake peers A then B on a fresh random hash:
//!   how long until B sees A. The same test on the mainline DHT (BEP 5).
//!
//! Two PCs: cargo run -p uc-core --example tracker_probe -- <CLÉ> <NOM>
//!   Same key on both PCs, started at about the same time. STUN, port opening
//!   on the router (NAT-PMP / UPnP, as the app does), announce on every
//!   tracker and on the DHT, then hellos to every address found.
//!   Prints when each source finds the other PC and when the punch works.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use mainline::{Dht, Id};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// (host:port, was in SmartSteamEmu's default list).
const TRACKERS: &[(&str, bool)] = &[
    ("tracker.opentrackr.org:1337", false),
    ("open.demonii.com:1337", true),
    ("open.stealth.si:80", false),
    ("tracker.torrent.eu.org:451", false),
    ("exodus.desync.com:6969", false),
    ("explodie.org:6969", false),
    ("tracker.theoks.net:6969", false),
    ("tracker-udp.gbitt.info:80", false),
    ("opentracker.io:6969", false),
    ("tracker.dler.org:6969", false),
    ("tracker.qu.ax:6969", false),
    ("wepzone.net:6969", false),
    ("tracker.openbittorrent.com:80", true),
    ("tracker.coppersurfer.tk:6969", true),
    ("tracker.leechers-paradise.org:6969", true),
    ("tracker.internetwarriors.net:1337", true),
];
const STUN: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// BEP 15 magic for the connect request.
const PROTOCOL_ID: u64 = 0x0417_2710_1980;
/// Short on purpose (BEP 15 says 15 s): we measure who answers fast.
const TIMEOUT: Duration = Duration::from_secs(2);
const TRIES: usize = 3;
/// Two-PC mode: trackers never push, the first one there only sees the
/// second at its next announce. So: every 3 s for the first minute, then
/// every 15 s (SSE's period).
const REANNOUNCE_FAST: Duration = Duration::from_secs(3);
const FAST_PHASE: Duration = Duration::from_secs(60);
const REANNOUNCE: Duration = Duration::from_secs(15);
const RUN_FOR: Duration = Duration::from_secs(180);
const HELLO: &[u8] = b"UCTP1 ";

const EV_NONE: u32 = 0;
const EV_STARTED: u32 = 2;
const EV_STOPPED: u32 = 3;

static T0: OnceLock<Instant> = OnceLock::new();

fn log(m: impl AsRef<str>) {
    let t = T0.get_or_init(Instant::now).elapsed().as_secs_f32();
    println!("[+{t:>6.2}s] {}", m.as_ref());
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn peer_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(b"-UC0001-");
    id[8..].copy_from_slice(&rand::random::<[u8; 12]>());
    id
}

async fn resolve(host: &str) -> Result<SocketAddr> {
    tokio::net::lookup_host(host).await.context("DNS")?.find(|a| a.is_ipv4()).context("pas d'adresse IPv4")
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
            let (n, from) = r.context("port fermé (ICMP)")?;
            let b = &buf[..n];
            if from != to || n < 8 || be32(&b[4..8]) != tx {
                continue;
            }
            match be32(&b[0..4]) {
                a if a == action => return Ok(b.to_vec()),
                3 => bail!("refus du tracker : {}", String::from_utf8_lossy(&b[8..])),
                _ => continue,
            }
        }
    }
    bail!("pas de réponse ({TRIES} essais de {} s)", TIMEOUT.as_secs())
}

struct Announce {
    /// DNS + connect round trip.
    connect: Duration,
    announce: Duration,
    interval: u32,
    peers: Vec<SocketAddrV4>,
}

/// One BEP 15 connect + announce. `port` is the port others should use.
async fn announce(sock: &UdpSocket, host: &str, hash: &[u8; 20], pid: &[u8; 20], port: u16, event: u32) -> Result<Announce> {
    let t = Instant::now();
    let to = resolve(host).await?;
    let tx: u32 = rand::random();
    let mut c = Vec::with_capacity(16);
    c.extend(PROTOCOL_ID.to_be_bytes());
    c.extend(0u32.to_be_bytes());
    c.extend(tx.to_be_bytes());
    let r = request(sock, to, &c, tx, 0).await.context("connexion")?;
    if r.len() < 16 {
        bail!("réponse de connexion trop courte");
    }
    let connect = t.elapsed();

    let t = Instant::now();
    let tx: u32 = rand::random();
    let mut a = Vec::with_capacity(98);
    a.extend(&r[8..16]); // connection id
    a.extend(1u32.to_be_bytes()); // action: announce
    a.extend(tx.to_be_bytes());
    a.extend(hash);
    a.extend(pid);
    a.extend(0u64.to_be_bytes()); // downloaded
    a.extend(1u64.to_be_bytes()); // left: a leecher gets every peer, a seeder may not
    a.extend(0u64.to_be_bytes()); // uploaded
    a.extend(event.to_be_bytes());
    a.extend(0u32.to_be_bytes()); // ip: use the sender's
    a.extend(rand::random::<u32>().to_be_bytes()); // key
    a.extend((-1i32).to_be_bytes()); // num_want: default
    a.extend(port.to_be_bytes());
    let r = request(sock, to, &a, tx, 1).await.context("annonce")?;
    if r.len() < 20 {
        bail!("réponse d'annonce trop courte");
    }
    let peers = r[20..]
        .chunks_exact(6)
        .map(|p| SocketAddrV4::new(Ipv4Addr::new(p[0], p[1], p[2], p[3]), u16::from_be_bytes([p[4], p[5]])))
        .collect();
    Ok(Announce { connect, announce: t.elapsed(), interval: be32(&r[8..12]), peers })
}

fn label(host: &str, sse: bool) -> String {
    format!("{host}{}", if sse { " (SSE)" } else { "" })
}

// ---------------------------------------------------------------- solo

/// A announces, then B announces (retrying twice): does B get A back?
async fn solo_tracker(host: &str, hash: [u8; 20]) -> Result<String> {
    let (a, b) = (UdpSocket::bind("0.0.0.0:0").await?, UdpSocket::bind("0.0.0.0:0").await?);
    let port_a = 20_000 + rand::random::<u16>() % 40_000;
    let port_b = port_a + 1;
    let ra = announce(&a, host, &hash, &peer_id(), port_a, EV_STARTED).await.context("A")?;
    let pid_b = peer_id();
    let mut rb = announce(&b, host, &hash, &pid_b, port_b, EV_STARTED).await.context("B")?;
    let meet = rb.connect + rb.announce;
    let mut tries = 1;
    while !rb.peers.iter().any(|p| p.port() == port_a) && tries < 3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rb = announce(&b, host, &hash, &pid_b, port_b, EV_NONE).await.context("B")?;
        tries += 1;
    }
    let seen = rb.peers.iter().any(|p| p.port() == port_a);
    Ok(format!(
        "DNS+connexion {:>7} · annonce {:>7} · B voit A : {} · rendez-vous {:>7} · intervalle {} s",
        ms(ra.connect),
        ms(ra.announce),
        match (seen, tries) {
            (false, _) => "NON".to_string(),
            (true, 1) => "oui".to_string(),
            (true, n) => format!("oui ({n}e essai)"),
        },
        ms(meet),
        rb.interval
    ))
}

/// Same scenario on the mainline DHT (BEP 5 announce_peer / get_peers).
async fn solo_dht() -> Result<String> {
    let t = Instant::now();
    let (a, b) = (Dht::client()?.as_async(), Dht::client()?.as_async());
    let (ok_a, ok_b) = tokio::join!(a.bootstrapped(), b.bootstrapped());
    if !ok_a || !ok_b {
        bail!("démarrage du DHT impossible");
    }
    let boot = t.elapsed();
    let hash = Id::random();
    let port = 20_000 + rand::random::<u16>() % 40_000;
    let t = Instant::now();
    a.announce_peer(hash, Some(port)).await.context("annonce")?;
    let ann = t.elapsed();
    let t = Instant::now();
    let found = loop {
        if t.elapsed() > Duration::from_secs(60) {
            break None;
        }
        let mut s = b.get_peers(hash);
        let mut hit = false;
        while let Some(peers) = s.next().await {
            if peers.iter().any(|p| p.port() == port) {
                hit = true;
                break;
            }
        }
        if hit {
            break Some(t.elapsed());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    Ok(format!(
        "démarrage {:>7} · annonce {:>7} · recherche {} · rendez-vous (annonce + recherche) {}",
        ms(boot),
        ms(ann),
        found.map(ms).unwrap_or_else(|| "NON trouvé en 60 s".into()),
        found.map(|f| ms(ann + f)).unwrap_or_else(|| "-".into()),
    ))
}

async fn solo() -> Result<()> {
    println!("Test de {} trackers UDP et du DHT (deux faux pairs A et B sur un hash aléatoire)…\n", TRACKERS.len());
    let hash: [u8; 20] = rand::random();
    let dht = tokio::spawn(solo_dht());
    let lines = futures::future::join_all(TRACKERS.iter().map(|&(host, sse)| async move {
        match solo_tracker(host, hash).await {
            Ok(s) => format!("{:<42} {s}", label(host, sse)),
            Err(e) => format!("{:<42} ÉCHEC : {e:#}", label(host, sse)),
        }
    }))
    .await;
    for l in lines {
        println!("{l}");
    }
    println!("\nDHT (en cours, jusqu'à ~1 min)…");
    match dht.await? {
        Ok(s) => println!("{:<42} {s}", "DHT mainline (BEP 5)"),
        Err(e) => println!("{:<42} ÉCHEC : {e:#}", "DHT mainline (BEP 5)"),
    }
    Ok(())
}

// ---------------------------------------------------------------- two PCs

type Found = mpsc::UnboundedSender<(String, SocketAddrV4)>;

async fn tracker_loop(host: &'static str, sse: bool, hash: [u8; 20], pid: [u8; 20], port: u16, tx: Found) {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else { return };
    let name = label(host, sse);
    let (mut said_ok, mut said_err) = (false, false);
    let mut event = EV_STARTED;
    loop {
        match announce(&sock, host, &hash, &pid, port, event).await {
            Ok(r) => {
                if !said_ok {
                    log(format!("{name} : annoncé en {} ({} pair(s) déjà là)", ms(r.connect + r.announce), r.peers.len()));
                    said_ok = true;
                }
                event = EV_NONE;
                for p in r.peers {
                    let _ = tx.send((host.to_string(), p));
                }
            }
            Err(e) if !said_err && !said_ok => {
                log(format!("{name} : ÉCHEC {e:#}"));
                said_err = true;
            }
            Err(_) => {}
        }
        let fast = T0.get().is_some_and(|t| t.elapsed() < FAST_PHASE);
        tokio::time::sleep(if fast { REANNOUNCE_FAST } else { REANNOUNCE }).await;
    }
}

async fn dht_loop(hash: [u8; 20], port: u16, tx: Found) {
    let run = async {
        let dht = Dht::client()?.as_async();
        dht.bootstrapped().await;
        log("DHT : démarré");
        let id = Id::from_bytes(hash)?;
        let mut last = None::<Instant>;
        loop {
            if last.is_none_or(|l| l.elapsed() > Duration::from_secs(60)) {
                let t = Instant::now();
                match dht.announce_peer(id, Some(port)).await {
                    Ok(_) if last.is_none() => log(format!("DHT : annoncé en {}", ms(t.elapsed()))),
                    Err(e) => log(format!("DHT : annonce en échec ({e})")),
                    _ => {}
                }
                last = Some(Instant::now());
            }
            let mut s = dht.get_peers(id);
            while let Some(peers) = s.next().await {
                for p in peers {
                    let _ = tx.send(("DHT".to_string(), p));
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    if let Err(e) = run.await {
        log(format!("DHT : ÉCHEC {e:#}"));
    }
}

async fn duo(key: &str, name: &str) -> Result<()> {
    log(format!("« {name} », clé « {key} »"));
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let public = uc_core::punch::stun(&sock, STUN).await?;
    let local_port = sock.local_addr()?.port();
    log(format!("STUN : adresse publique {public} (port local {local_port})"));

    // Same as the app: open the port on the router (NAT-PMP / UPnP) when it
    // can, and announce the opened port; otherwise the STUN port.
    let public_ip = match public.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        _ => None,
    };
    let lan_ip = uc_core::punch::lan_ipv4();
    let map = match uc_core::portmap::PortMap::open(lan_ip, local_port, public_ip).await {
        Ok(m) => {
            log(format!("Port ouvert sur la box par {} : {}", m.method, m.external));
            Some(m)
        }
        Err(e) => {
            log(format!("Ouverture de port impossible ({e:#}) : on annonce le port STUN"));
            None
        }
    };
    let announced = SocketAddr::V4(map.as_ref().map(|m| m.external).unwrap_or(match public {
        SocketAddr::V4(a) => a,
        SocketAddr::V6(_) => bail!("STUN a renvoyé une adresse IPv6"),
    }));
    let public = announced;

    let digest = Sha256::digest(format!("uc-tracker-probe:v1:{key}").as_bytes());
    let hash: [u8; 20] = digest[..20].try_into()?;
    let pid = peer_id();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for &(host, sse) in TRACKERS {
        tokio::spawn(tracker_loop(host, sse, hash, pid, public.port(), tx.clone()));
    }
    tokio::spawn(dht_loop(hash, public.port(), tx.clone()));

    let hello = [HELLO, name.as_bytes()].concat();
    let mut targets: HashSet<SocketAddr> = HashSet::new();
    let mut seen: HashSet<(SocketAddr, String)> = HashSet::new();
    let mut first_by: HashMap<String, f32> = HashMap::new();
    let mut punched: HashMap<SocketAddr, String> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let end = tokio::time::sleep(RUN_FOR);
    tokio::pin!(end);
    let mut buf = [0u8; 1500];
    log(format!("Recherche pendant {} s (Ctrl+C pour arrêter)…", RUN_FOR.as_secs()));
    loop {
        tokio::select! {
            _ = &mut end => break,
            _ = tokio::signal::ctrl_c() => break,
            Some((src, p)) = rx.recv() => {
                let a = SocketAddr::V4(p);
                // Some trackers pad replies with junk such as 0.0.0.0:65535.
                let ip = p.ip();
                if a == public || p.port() == 0 || ip.is_unspecified() || ip.is_loopback() || ip.is_broadcast() || ip.is_multicast() {
                    continue;
                }
                if seen.insert((a, src.clone())) {
                    let first = targets.insert(a);
                    first_by.entry(src.clone()).or_insert_with(|| T0.get().unwrap().elapsed().as_secs_f32());
                    log(format!("{src} : pair {a}{}", if first { " (nouveau)" } else { "" }));
                }
            }
            _ = tick.tick() => {
                for t in &targets {
                    let _ = sock.send_to(&hello, t).await;
                }
            }
            r = sock.recv_from(&mut buf) => {
                // Errors here are ICMP replies from stale addresses: ignore.
                let Ok((n, from)) = r else { continue };
                if let Some(who) = buf[..n].strip_prefix(HELLO) {
                    let who = String::from_utf8_lossy(who).to_string();
                    if !punched.contains_key(&from) {
                        log(format!("PERCÉ : « {who} » répond depuis {from}"));
                        punched.insert(from, who);
                    }
                    targets.insert(from);
                }
            }
        }
    }

    println!("\n--- Résumé ---");
    let mut by: Vec<_> = first_by.into_iter().collect();
    by.sort_by(|a, b| a.1.total_cmp(&b.1));
    for (src, t) in by {
        println!("{src:<42} premier pair vu à +{t:.1} s");
    }
    if punched.is_empty() {
        println!("Aucune connexion directe établie.");
    }
    for (a, who) in &punched {
        println!("Connecté à « {who} » via {a}");
    }
    // Leave the swarms so the next run does not see this one's port.
    let stops = TRACKERS.iter().map(|&(host, _)| async move {
        if let Ok(s) = UdpSocket::bind("0.0.0.0:0").await {
            let _ = tokio::time::timeout(Duration::from_secs(3), announce(&s, host, &hash, &pid, public.port(), EV_STOPPED)).await;
        }
    });
    futures::future::join_all(stops).await;
    if let Some(m) = map {
        m.close().await;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    T0.get_or_init(Instant::now);
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => solo().await,
        [key, name] => duo(key, name).await,
        _ => bail!("usage : tracker_probe            (test solo)\n        tracker_probe <CLÉ> <NOM> (sur deux PC)"),
    }
}
