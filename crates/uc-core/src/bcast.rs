//! Relays this PC's LAN game announcements into the game salon.
//!
//! Games without an IP setting announce their lobby with UDP broadcasts to
//! 255.255.255.255 from an unbound socket: Windows sends them out of ONE
//! adapter of its choice (often a VPN's such as Radmin, which takes the best
//! metric on purpose). EasyTier forwards neither those nor the ones sent from
//! our own adapter. A game bound to our virtual IP is no better off: Windows
//! may still send its broadcast through its preferred adapter, keeping the
//! virtual IP as source. So we watch every local adapter (raw socket in
//! SIO_RCVALL mode, needs admin) for broadcasts sent from any of this PC's
//! addresses, and send each
//! one again into the salon as a subnet broadcast (10.X.255.255, which
//! EasyTier does forward), from our virtual IP and the game's port: other
//! players' games answer us through the salon.

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// System chatter that has nothing to do in a game salon: DHCP, NetBIOS,
/// SSDP, mDNS, WS-Discovery, Dropbox LAN sync.
const IGNORED_PORTS: &[u16] = &[67, 68, 137, 138, 1900, 3702, 5353, 17500];
/// Relayed packets per second, at most (a broken game must not flood).
const MAX_RATE: u32 = 200;
/// New adapters (ours appears a few seconds after the session starts).
const RESCAN: Duration = Duration::from_secs(5);
/// The same announcement seen twice within this delay (on two adapters, or
/// sent and looped back) is relayed once.
const DEDUP: Duration = Duration::from_millis(100);

pub struct BroadcastRelay {
    stop: Arc<AtomicBool>,
    pub relayed: Arc<AtomicU64>,
}

impl Drop for BroadcastRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Shared {
    stop: Arc<AtomicBool>,
    relayed: Arc<AtomicU64>,
    out: Socket,
    vip: Ipv4Addr,
    target: Ipv4Addr,
    /// (second, packets in that second): rate limit.
    rate: Mutex<(u64, u32)>,
    /// Destination ports already reported (logged once).
    seen_ports: Mutex<HashSet<u16>>,
    /// This PC's IPv4 addresses: broadcasts from them are ours to relay.
    local: Mutex<HashSet<Ipv4Addr>>,
    /// Recently relayed announcements (time, fingerprint).
    recent: Mutex<VecDeque<(Instant, u64)>>,
}

impl BroadcastRelay {
    /// `vip`: our virtual IP; announcements go to 10.<subnet>.255.255.
    pub fn start(vip: Ipv4Addr) -> Result<Self> {
        let [_, subnet, _, _] = vip.octets();
        // Raw IPv4 sender: we write the IP header (source = our virtual IP,
        // which exists on our adapter, so Windows accepts it).
        let out = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::UDP)).context("socket brut")?;
        out.set_header_included_v4(true)?;
        out.set_broadcast(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let relayed = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared {
            stop: stop.clone(),
            relayed: relayed.clone(),
            out,
            vip,
            target: Ipv4Addr::new(10, subnet, 255, 255),
            rate: Mutex::new((0, 0)),
            seen_ports: Mutex::new(HashSet::new()),
            local: Mutex::new(HashSet::from([vip])),
            recent: Mutex::new(VecDeque::new()),
        });
        std::thread::spawn(move || watch_adapters(shared));
        Ok(Self { stop, relayed })
    }
}

/// A failed capture is tried again after this delay.
const RETRY: Duration = Duration::from_secs(30);

/// One capture thread per local IPv4 address, started as addresses appear.
/// Keyed by (address, adapter index): an adapter recreated with the same
/// address (EasyTier restarted) gets a new index, hence a new capture.
fn watch_adapters(shared: Arc<Shared>) {
    // Per capture: cleared by the thread when it ends, or by us to stop it.
    let mut watched: HashMap<(Ipv4Addr, Option<u32>), Arc<AtomicBool>> = HashMap::new();
    let mut failed: HashMap<(Ipv4Addr, Option<u32>), Instant> = HashMap::new();
    while !shared.stop.load(Ordering::Relaxed) {
        let addrs = if_addrs::get_if_addrs().unwrap_or_default();
        {
            let mut local = shared.local.lock().unwrap();
            local.extend(addrs.iter().filter_map(|a| match a.ip() {
                std::net::IpAddr::V4(ip) => Some(ip),
                _ => None,
            }));
        }
        let current: HashSet<(Ipv4Addr, Option<u32>)> = addrs
            .iter()
            .filter_map(|a| match a.ip() {
                std::net::IpAddr::V4(ip) => Some((ip, a.index)),
                _ => None,
            })
            .collect();
        // Adapter gone (or recreated), or capture ended: watch it again later.
        watched.retain(|key, alive| {
            let keep = current.contains(key) && alive.load(Ordering::Relaxed);
            if !keep {
                alive.store(false, Ordering::Relaxed);
            }
            keep
        });
        for a in addrs {
            let std::net::IpAddr::V4(ip) = a.ip() else { continue };
            let key = (ip, a.index);
            if ip.is_loopback() || ip.is_link_local() || watched.contains_key(&key) {
                continue;
            }
            if failed.get(&key).is_some_and(|t| t.elapsed() < RETRY) {
                continue;
            }
            // Ours (Wintun) only carries IP packets: capture at the IP level.
            let ip_level = a.name.starts_with("UC-");
            match capture_socket(ip, ip_level) {
                Ok(sock) => {
                    tracing::info!("annonces de jeux : écoute de la carte « {} » ({ip})", a.name);
                    failed.remove(&key);
                    let alive = Arc::new(AtomicBool::new(true));
                    watched.insert(key, alive.clone());
                    let (shared, name) = (shared.clone(), a.name.clone());
                    std::thread::spawn(move || capture_loop(shared, alive, sock, ip, name));
                }
                Err(e) => {
                    // Logged once; retried quietly (an adapter coming up).
                    if failed.insert(key, Instant::now()).is_none() {
                        tracing::warn!("annonces de jeux : carte « {} » ({ip}) non écoutée : {e:#}", a.name);
                    }
                }
            }
        }
        std::thread::sleep(RESCAN);
    }
}

/// Raw socket receiving every IPv4 packet of the adapter owning `ip`,
/// outgoing ones included.
fn capture_socket(ip: Ipv4Addr, ip_level: bool) -> Result<Socket> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{RCVALL_IPLEVEL, RCVALL_ON, SIO_RCVALL, WSAIoctl};
    let sock = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::from(0)))?;
    sock.bind(&SockAddr::from(SocketAddrV4::new(ip, 0)))?;
    sock.set_read_timeout(Some(Duration::from_secs(1)))?;
    let on: u32 = if ip_level { RCVALL_IPLEVEL } else { RCVALL_ON } as u32;
    let mut returned = 0u32;
    let r = unsafe {
        WSAIoctl(
            sock.as_raw_socket() as usize,
            SIO_RCVALL,
            &on as *const u32 as *const _,
            4,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 {
        anyhow::bail!("SIO_RCVALL refusé ({})", std::io::Error::last_os_error());
    }
    Ok(sock)
}

fn capture_loop(shared: Arc<Shared>, alive: Arc<AtomicBool>, sock: Socket, ip: Ipv4Addr, name: String) {
    let mut buf = vec![MaybeUninit::<u8>::uninit(); 65536];
    // Diagnostic: does this adapter's capture see what we send at all, and
    // which sources do our announcements leave it with?
    let mut saw_outgoing = false;
    let mut seen_sources: HashSet<Ipv4Addr> = HashSet::new();
    while !shared.stop.load(Ordering::Relaxed) && alive.load(Ordering::Relaxed) {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            // The adapter went away: the watcher opens a new capture if it
            // comes back.
            Err(_) => {
                alive.store(false, Ordering::Relaxed);
                return;
            }
        };
        // Every IP packet of the PC comes through here while a game runs:
        // looked at in place, nothing is copied.
        // SAFETY: `recv` initialized the first `n` bytes of `buf`.
        let pkt: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n) };
        if !saw_outgoing && pkt.len() >= 20 && pkt[12..16] == ip.octets() {
            saw_outgoing = true;
            tracing::info!("annonces de jeux : la carte « {name} » voit les paquets sortants");
        }
        let is_local = |a: Ipv4Addr| shared.local.lock().unwrap().contains(&a);
        if let Some((src, src_port, dst_port, payload)) = outgoing_broadcast(pkt, is_local) {
            if seen_sources.insert(src) {
                tracing::info!("annonces de jeux : annonce de {src} (port {dst_port}) vue sur la carte « {name} »");
            }
            relay(&shared, src_port, dst_port, payload);
        }
    }
}

/// A UDP broadcast to 255.255.255.255 sent from one of this PC's addresses:
/// (source, source port, destination port, payload).
fn outgoing_broadcast(pkt: &[u8], is_local: impl Fn(Ipv4Addr) -> bool) -> Option<(Ipv4Addr, u16, u16, &[u8])> {
    if pkt.len() < 28 || pkt[0] >> 4 != 4 || pkt[9] != 17 {
        return None;
    }
    let ihl = (pkt[0] & 0x0f) as usize * 4;
    let src = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
    let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
    // Fragments are rare for announcements: ignored.
    let fragmented = (pkt[6] & 0x20) != 0 || (u16::from_be_bytes([pkt[6], pkt[7]]) & 0x1fff) != 0;
    if dst != Ipv4Addr::BROADCAST || fragmented || pkt.len() < ihl + 8 || !is_local(src) {
        return None;
    }
    let udp = &pkt[ihl..];
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    let len = (u16::from_be_bytes([udp[4], udp[5]]) as usize).clamp(8, udp.len());
    if IGNORED_PORTS.contains(&dst_port) || IGNORED_PORTS.contains(&src_port) {
        return None;
    }
    Some((src, src_port, dst_port, &udp[8..len]))
}

fn relay(shared: &Shared, src_port: u16, dst_port: u16, payload: &[u8]) {
    {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (src_port, dst_port, payload).hash(&mut h);
        let print = h.finish();
        let mut recent = shared.recent.lock().unwrap();
        while recent.front().is_some_and(|(t, _)| t.elapsed() > DEDUP) {
            recent.pop_front();
        }
        if recent.iter().any(|(_, p)| *p == print) {
            return;
        }
        recent.push_back((Instant::now(), print));
    }
    let second = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    {
        let mut rate = shared.rate.lock().unwrap();
        if rate.0 != second {
            *rate = (second, 0);
        }
        if rate.1 >= MAX_RATE {
            return;
        }
        rate.1 += 1;
    }
    if shared.seen_ports.lock().unwrap().insert(dst_port) {
        tracing::info!("annonces de jeux : relais vers le salon des annonces du port {dst_port}");
    }
    let pkt = udp_packet(shared.vip, shared.target, src_port, dst_port, payload);
    let dst = SockAddr::from(SocketAddrV4::new(shared.target, dst_port));
    if shared.out.send_to(&pkt, &dst).is_ok() {
        shared.relayed.fetch_add(1, Ordering::Relaxed);
    }
}

/// IPv4 + UDP packet (UDP checksum 0: optional in IPv4).
fn udp_packet(src: Ipv4Addr, dst: Ipv4Addr, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let total = 20 + 8 + payload.len();
    let mut p = Vec::with_capacity(total);
    p.extend([0x45, 0, (total >> 8) as u8, total as u8]);
    p.extend(rand::random::<u16>().to_be_bytes());
    p.extend([0x40, 0, 64, 17, 0, 0]); // don't fragment, TTL 64, UDP, checksum
    p.extend(src.octets());
    p.extend(dst.octets());
    let sum = checksum(&p);
    p[10..12].copy_from_slice(&sum.to_be_bytes());
    p.extend(src_port.to_be_bytes());
    p.extend(dst_port.to_be_bytes());
    p.extend(((8 + payload.len()) as u16).to_be_bytes());
    p.extend([0, 0]);
    p.extend(payload);
    p
}

fn checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = header.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_parses_a_broadcast() {
        let ip = Ipv4Addr::new(26, 1, 2, 3);
        let mut pkt = udp_packet(ip, Ipv4Addr::BROADCAST, 4000, 5000, b"lobby");
        assert_eq!(checksum(&pkt[..20]), 0, "en-tête IP valide");
        let ours = |a: Ipv4Addr| a == ip;
        assert_eq!(outgoing_broadcast(&pkt, ours), Some((ip, 4000, 5000, &b"lobby"[..])));
        // Someone else's broadcast, or system chatter: ignored.
        assert_eq!(outgoing_broadcast(&pkt, |a| a == Ipv4Addr::new(26, 1, 2, 4)), None);
        pkt[22..24].copy_from_slice(&138u16.to_be_bytes());
        assert_eq!(outgoing_broadcast(&pkt, ours), None);
    }
}
