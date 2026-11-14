//! A running room session: discovery + hole punching + EasyTier + status.

use crate::cache::{self, SalonCache};
use crate::chat::{Chat, ChatEvent};
use crate::ipc::Event;
use crate::discovery::{self, Discovery, PeerRecord};
use crate::easytier::{self, CoreConfig, EasyTier};
use crate::identity::Identity;
use crate::punch::{self, PunchEvent, Puncher};
use crate::room::Room;
use crate::system::FirewallRule;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const STUN_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// How long the UI says "looking for players" before "nobody online".
const SEARCH_PHASE: Duration = Duration::from_secs(60);
/// Network change checks (see `watch_network`), and every how many of them
/// the public address is checked too.
const NET_CHECK_EVERY: Duration = Duration::from_secs(5);
const NET_STUN_EVERY: u64 = 24;
/// While the session starts, EasyTier's peer list is read every second.
const CLI_FAST_PHASE: Duration = Duration::from_secs(60);
/// DHT reads: fast while the salon forms, then this often (a newcomer
/// probing us also triggers a read right away).
const DISCOVERY_FAST: Duration = Duration::from_secs(10);
const DISCOVERY_SLOW: Duration = Duration::from_secs(15);
/// Minimum delay between two reads triggered by newcomers.
const DISCOVERY_POKE_MIN: Duration = Duration::from_secs(3);
/// EasyTier is restarted if it stops, up to this many times in the window.
const ET_MAX_RESTARTS: usize = 3;
const ET_RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);
/// The same unknown peer triggers a read at most this often.
const POKE_PER_PEER: Duration = Duration::from_secs(30);
/// A member announced through Nostr is not removed for missing from the DHT
/// during this delay (its DHT record may not be readable yet).
const PUSHED_KEEP: Duration = Duration::from_secs(90);
/// Port opening is awaited this long before our record goes out (NAT-PMP
/// answers in a few ms; a box that never answers takes about 5 s).
const PORTMAP_WAIT: Duration = Duration::from_secs(1);

/// What other members see of us; changes apply live.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub name: String,
    /// "emoji|#rrggbb".
    pub avatar: String,
}

/// Requests from the helper to a running session.
pub enum Ctl {
    Say(String),
    WantImage(String),
    SalonChanged,
    SetBanned(Vec<String>),
    /// Game mode: test the virtual LAN (see `gametest`).
    GameTest,
}

pub struct SessionConfig {
    pub salon_id: String,
    pub access_key: [u8; 32],
    pub profile: watch::Receiver<Profile>,
    /// Game mode: virtual adapter + full LAN. Otherwise chat/presence only.
    pub game: bool,
    /// Member ids refused by the chat.
    pub banned: Vec<String>,
    /// Chat lines of this salon, tagged with its id.
    /// Chat lines, received images, salon change hints (forwarded to the app).
    pub events_out: mpsc::UnboundedSender<Event>,
    pub relays: Vec<String>,
    /// Second octet of the salon subnet: 10.<subnet>.0.0/16 (see `salons::subnet_for`).
    pub subnet: u8,
    pub fixed_ip: Option<Ipv4Addr>,
    /// Allow LAN shortcuts; disable to test the Internet path from one LAN.
    pub use_lan: bool,
    /// Ask the router to open EasyTier's port (NAT-PMP / UPnP, see `portmap`).
    pub port_map: bool,
    /// Fixed EasyTier port, forwarded by hand on the router (0 = automatic).
    pub custom_port: u16,
    pub data_dir: PathBuf,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Member {
    /// Member id ("" when unknown yet).
    #[serde(default)]
    pub id: String,
    /// Chat link with this member is up.
    #[serde(default)]
    pub chat: bool,
    pub name: String,
    #[serde(default)]
    pub avatar: String,
    pub vip: String,
    pub latency_ms: String,
    pub loss: String,
    pub path: String,
    pub nat: String,
    /// Relayed: the member our traffic to this one goes through ("" if direct).
    #[serde(default)]
    pub relay_via: String,
    /// Ping measured through the virtual adapter (game mode only).
    #[serde(default)]
    pub ping: Option<crate::gametest::PingStats>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub salon_id: String,
    /// Has the virtual adapter (see `SessionConfig::game`).
    #[serde(default)]
    pub game: bool,
    /// Connection progress: "dht", "starting", "searching" (first minute
    /// after the network is up), then "ready".
    #[serde(default)]
    pub stage: String,
    pub my_name: String,
    pub my_vip: Option<Ipv4Addr>,
    pub public_punch: Option<SocketAddr>,
    pub dht_slot: Option<usize>,
    pub members: Vec<Member>,
    pub events: Vec<String>,
    /// Last game network test (game mode only).
    #[serde(default)]
    pub game_test: Option<crate::gametest::GameTest>,
    /// Connection milestones with their time since the session started
    /// (kept whole, unlike `events`): where the waiting goes.
    #[serde(default)]
    pub timings: Vec<String>,
    /// How others can reach us: "NAT-PMP" / "UPnP" when our router opened
    /// our port, "" otherwise.
    #[serde(default)]
    pub port_open: String,
    /// Our NAT, from STUN: "cone", "symmetric", or "" (unknown).
    #[serde(default)]
    pub nat: String,
    /// Each player's path as it changes (direct, punched, relayed...), with
    /// the local time to compare with the other players' diagnostics.
    #[serde(default)]
    pub path_log: Vec<String>,
}

pub struct Session {
    shutdown: Option<oneshot::Sender<()>>,
    ctl: mpsc::UnboundedSender<Ctl>,
    pub status: watch::Receiver<Status>,
    task: JoinHandle<Result<()>>,
}

impl Session {
    pub fn start(cfg: SessionConfig) -> Self {
        let (status_tx, status) = watch::channel(Status::default());
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let (mut shutdown_rx, mut ctl_rx) = (shutdown_rx, ctl_rx);
            // Bans received during a session are kept for the next one.
            let mut banned = cfg.banned.clone();
            loop {
                match run(&cfg, &mut banned, status_tx.clone(), &mut shutdown_rx, &mut ctl_rx).await {
                    Ok(Exit::Stopped) => return Ok(()),
                    // Everything (addresses, port, paths) is rebuilt by a new
                    // session, the same way members find each other at start.
                    Ok(Exit::NetworkChanged(why)) => {
                        status_tx.send_modify(|s| {
                            push_event(s, Instant::now(), format!("Réseau changé ({why}) : reconnexion au salon"));
                            s.timings.clear();
                        });
                        tokio::select! {
                            _ = &mut shutdown_rx => return Ok(()),
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                    }
                    Err(e) => {
                        status_tx.send_modify(|s| push_event(s, Instant::now(), format!("ERREUR : {e:#}")));
                        return Err(e);
                    }
                }
            }
        });
        Self { shutdown: Some(shutdown), ctl, status, task }
    }

    /// Sends a chat message to the salon.
    pub fn say(&self, text: String) {
        let _ = self.ctl.send(Ctl::Say(text));
    }

    /// Asks members for a missing image (custom avatar).
    pub fn want_image(&self, hash: String) {
        let _ = self.ctl.send(Ctl::WantImage(hash));
    }

    /// Owner: tells members to refetch the salon manifest now.
    pub fn notify_salon_changed(&self) {
        let _ = self.ctl.send(Ctl::SalonChanged);
    }

    pub fn game_test(&self) {
        let _ = self.ctl.send(Ctl::GameTest);
    }

    pub fn set_banned(&self, banned: Vec<String>) {
        let _ = self.ctl.send(Ctl::SetBanned(banned));
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.task.await?
    }
}

fn push_event(s: &mut Status, t0: Instant, msg: String) {
    tracing::info!("{msg}");
    s.events.push(format!("[+{:>4}s] {msg}", t0.elapsed().as_secs()));
    let n = s.events.len();
    if n > 8 {
        s.events.drain(..n - 8);
    }
}

/// Most recent path changes kept in `Status::path_log`.
const PATH_LOG_MAX: usize = 60;

/// Local wall-clock time "HH:MM:SS".
fn local_hms() -> String {
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

/// Records a milestone once (`key` identifies it).
/// Result of a port opening still running (pending forever once taken).
async fn finished<T>(task: &mut Option<JoinHandle<T>>) -> Option<Result<T, tokio::task::JoinError>> {
    match task {
        Some(t) => {
            let r = t.await;
            *task = None;
            Some(r)
        }
        None => std::future::pending().await,
    }
}

/// Reports the opening of EasyTier's port on the router.
fn port_map_done(
    r: Result<Result<crate::portmap::PortMap>, tokio::task::JoinError>,
    ev: &dyn Fn(String),
    status: &watch::Sender<Status>,
    t0: Instant,
    marks: &mut HashSet<&'static str>,
) -> Option<crate::portmap::PortMap> {
    match r {
        Ok(Ok(pm)) => {
            ev(format!("Port ouvert sur la box ({}) : joignable directement à {}", pm.method, pm.external));
            mark(status, t0, marks, "portmap", format!("port ouvert sur la box ({})", pm.method));
            status.send_modify(|s| s.port_open = pm.method.to_string());
            Some(pm)
        }
        Ok(Err(e)) => {
            ev(format!("Ouverture automatique du port impossible : {e:#}"));
            None
        }
        Err(_) => None,
    }
}

fn mark(status: &watch::Sender<Status>, t0: Instant, done: &mut HashSet<&'static str>, key: &'static str, label: String) {
    if done.insert(key) {
        let line = format!("{:>5.1} s  {label}", t0.elapsed().as_secs_f32());
        tracing::info!("chrono : {line}");
        status.send_modify(|s| s.timings.push(line));
    }
}

/// Stable virtual IP derived from our id, avoiding addresses already taken.
fn pick_vip(id: &str, subnet: u8, others: &[PeerRecord]) -> Ipv4Addr {
    (0u32..)
        .map(|n| {
            let h = Sha256::new().chain_update(id).chain_update(n.to_le_bytes()).finalize();
            (h[0], h[1])
        })
        .filter(|&(_, b)| b != 0 && b != 255)
        .map(|(a, b)| Ipv4Addr::new(10, subnet, a, b))
        .find(|ip| !others.iter().any(|r| r.vip == *ip))
        .unwrap()
}

/// Runs at scope exit, whichever way `run` ends (errors included): tasks
/// and ports of a session must not outlive it in the helper.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

struct PeerState {
    rec: PeerRecord,
    /// Bridge URL EasyTier should dial (dialer side only).
    wanted: Option<String>,
    /// Bridge URL EasyTier actually accepted.
    connector: Option<String>,
    connector_error: Option<String>,
    /// Direct URLs EasyTier currently has for this member.
    direct: Vec<String>,
}

/// Discovery task -> session loop.
enum Disc {
    /// A member remembered from an earlier session.
    Cached(PeerRecord),
    /// A member found during a read (more may follow).
    Found(PeerRecord),
    /// A member's record pushed by a Nostr relay (name of the relay).
    Pushed(PeerRecord, String),
    /// A member's record received directly, from an address a tracker gave.
    Introduced(PeerRecord, SocketAddr),
    /// End of a read: every fresh member (others are gone).
    All(Vec<PeerRecord>, Option<usize>),
    Published(usize),
}

/// Reachable listeners of a member (UPnP, forwarded port, same LAN): joined
/// at once, from both sides. Follows the member's record (its addresses
/// change between sessions); retried while EasyTier's API is not ready.
async fn add_direct(et: &EasyTier, p: &mut PeerState) {
    if p.direct == p.rec.direct {
        return;
    }
    for url in std::mem::take(&mut p.direct) {
        if !p.rec.direct.contains(&url) {
            let _ = et.remove_connector(&url).await;
        } else {
            p.direct.push(url);
        }
    }
    for url in &p.rec.direct {
        if !p.direct.contains(url) && et.add_connector(url).await.is_ok() {
            p.direct.push(url.clone());
        }
    }
    // Same order as the record, so the comparison above settles.
    if p.rec.direct.iter().all(|u| p.direct.contains(u)) {
        p.direct = p.rec.direct.clone();
    }
}

/// A member left: EasyTier stops dialing its addresses.
async fn remove_links(et: &EasyTier, p: &PeerState) {
    if let Some(url) = &p.connector {
        let _ = et.remove_connector(url).await;
    }
    for url in &p.direct {
        let _ = et.remove_connector(url).await;
    }
}

/// Makes EasyTier's connector match `wanted`. EasyTier's API is not ready
/// for a few seconds after start (Wintun setup), so this is retried every tick.
async fn sync_connector(et: &EasyTier, p: &mut PeerState) -> Option<String> {
    if p.connector == p.wanted {
        return None;
    }
    if let Some(old) = p.connector.take() {
        let _ = et.remove_connector(&old).await;
    }
    let url = p.wanted.clone()?;
    match et.add_connector(&url).await {
        Ok(()) => {
            p.connector = Some(url.clone());
            p.connector_error = None;
            Some(format!("EasyTier connecté à la passerelle de {} ({url})", p.rec.name))
        }
        Err(e) => {
            // Report each distinct error once.
            let msg = format!("{e:#}").lines().next().unwrap_or_default().to_string();
            (p.connector_error.replace(msg.clone()) != Some(msg.clone()))
                .then(|| format!("connector add (nouvel essai dans 2 s) : {msg}"))
        }
    }
}

/// How a session ended without error.
enum Exit {
    Stopped,
    /// Our LAN or public address changed: the session must be rebuilt.
    NetworkChanged(String),
}

/// Checks the LAN address every few seconds (no packet sent) and the public
/// one by STUN every 2 minutes. A change seen twice in a row (not a blip)
/// is sent once, then the task ends.
async fn watch_network(lan: Ipv4Addr, public: Option<std::net::IpAddr>, tx: oneshot::Sender<String>) {
    // Test: a fake change after this many seconds.
    if let Some(secs) = std::env::var("UC_TEST_NET_CHANGE").ok().and_then(|v| v.parse().ok()) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let _ = tx.send("test UC_TEST_NET_CHANGE".into());
        return;
    }
    let mut tick = tokio::time::interval(NET_CHECK_EVERY);
    tick.tick().await;
    let mut suspect: Option<String> = None;
    for n in 1u64.. {
        tick.tick().await;
        let now_lan = punch::lan_ipv4();
        let mut why = None;
        // LOCALHOST: no route at the moment (cable out, Wi-Fi switching).
        if now_lan != lan && now_lan != Ipv4Addr::LOCALHOST {
            why = Some(format!("adresse locale {lan} → {now_lan}"));
        } else if let Some(old) = public.filter(|_| n % NET_STUN_EVERY == 0 || suspect.is_some()) {
            if let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                if let Ok(a) = punch::stun(&sock, STUN_SERVERS).await {
                    if a.ip() != old {
                        why = Some(format!("adresse publique {old} → {}", a.ip()));
                    }
                }
            }
        }
        match (why, suspect.is_some()) {
            (Some(w), true) => {
                let _ = tx.send(w);
                return;
            }
            (w, _) => suspect = w,
        }
    }
}

async fn run(
    cfg: &SessionConfig,
    banned: &mut Vec<String>,
    status: watch::Sender<Status>,
    shutdown: &mut oneshot::Receiver<()>,
    ctl_rx: &mut mpsc::UnboundedReceiver<Ctl>,
) -> Result<Exit> {
    let t0 = Instant::now();
    let ev = |msg: String| status.send_modify(|s| push_event(s, t0, msg));
    let mut marks = HashSet::new();

    let room = Arc::new(Room::new(&cfg.salon_id, &cfg.access_key));
    let ident = Identity::load_or_create(&cfg.data_dir)?;
    let mut profile = cfg.profile.clone();
    let my_name = || {
        let n = profile_name(&cfg.profile.borrow());
        if n.is_empty() { ident.name.clone() } else { n }
    };
    status.send_modify(|s| {
        s.salon_id = room.id.clone();
        s.game = cfg.game;
        s.stage = "dht".into();
        s.my_name = my_name();
    });
    if let Some(other) = cfg.game.then(|| crate::system::subnet_in_use(cfg.subnet)).flatten() {
        anyhow::bail!(
            "la plage 10.{}.0.0/16 de ce salon est déjà utilisée sur ce PC par « {other} » (autre VPN ?)",
            cfg.subnet
        );
    }

    // Same EasyTier port as last time when possible: members remember our
    // address (useful with UPnP / a forwarded port). The punch port, on the
    // contrary, is new each session: members still probing our previous
    // session's port leave NAT state behind that would block the same port
    // (the box then maps it to another public port and punching deadlocks).
    let mut salon_cache = SalonCache::load(&cfg.data_dir, &room.id);
    let lan_ip = punch::lan_ipv4();
    let sock = Puncher::bind().await?;
    // A port chosen by the user wins: it is the one forwarded on the router.
    let et_port = cache::port_or_free(if cfg.custom_port != 0 { cfg.custom_port } else { salon_cache.et_port })?;
    let custom_port_refused = cfg.custom_port != 0 && et_port != cfg.custom_port;
    salon_cache.et_port = et_port;
    salon_cache.save(&cfg.data_dir, &room.id);

    let stage = |name: &str| status.send_modify(|s| s.stage = name.to_string());
    // Nostr relays: connected now, our record is announced once known (see
    // `nostr`). UC_NO_NOSTR=1 compares with the DHT alone.
    let (rv_tx, mut rv_rx) = mpsc::unbounded_channel();
    let rendezvous = std::env::var_os("UC_NO_NOSTR")
        .is_none()
        .then(|| crate::nostr::Rendezvous::start(room.clone(), &ident.id, rv_tx.clone()));
    // Queries work during the DHT bootstrap: nothing waits for it.
    let disco = Arc::new(Discovery::new(room.clone(), &cfg.data_dir)?);
    let dht_check = {
        let (disco, status, data_dir) = (disco.clone(), status.clone(), cfg.data_dir.clone());
        tokio::spawn(async move {
            if disco.bootstrapped().await {
                let line = format!("{:>5.1} s  DHT prête (bootstrap)", t0.elapsed().as_secs_f32());
                status.send_modify(|s| s.timings.push(line));
                // Warmer start next time.
                tokio::time::sleep(Duration::from_secs(30)).await;
                disco.save_nodes(&data_dir).await;
            } else {
                status.send_modify(|s| push_event(s, t0, "DHT injoignable : seuls les membres déjà connus peuvent être trouvés".into()));
            }
        })
    };
    let _stop_dht_check = OnDrop({
        let h = dht_check.abort_handle();
        move || h.abort()
    });
    let public_punch = punch::stun(&sock, STUN_SERVERS).await.ok();
    match public_punch {
        Some(a) => ev(format!("Adresse publique (STUN) : {a}")),
        None => ev("STUN indisponible : perçage NAT limité au LAN".into()),
    }
    // Same socket, another server: a different public port means a
    // symmetric NAT (hole punching fails unless the other side is open).
    if let (Some(a), Ok(b)) = (public_punch, punch::stun(&sock, &STUN_SERVERS[1..]).await) {
        status.send_modify(|s| s.nat = if a == b { "cone" } else { "symmetric" }.into());
        let nat = if a == b {
            "NAT de type cône : le perçage est possible".to_string()
        } else {
            format!("NAT SYMÉTRIQUE ({a} puis {b}) : le perçage échouera avec la plupart des autres joueurs, un relais sera nécessaire")
        };
        ev(nat.clone());
        status.send_modify(|s| s.timings.push(format!("        {nat}")));
    }
    status.send_modify(|s| s.public_punch = public_punch);
    mark(&status, t0, &mut marks, "stun", "STUN terminé".into());
    if custom_port_refused {
        ev(format!(
            "Port choisi {} indisponible (déjà utilisé, par un autre salon ?) : port {et_port} utilisé à la place",
            cfg.custom_port
        ));
    }
    // Our router opens EasyTier's port: members then join us directly, even
    // where hole punching fails between two networks. Runs while EasyTier
    // starts; our record waits for it (a few ms with NAT-PMP).
    let port_map = cfg.port_map.then(|| {
        let public_ip = public_punch.and_then(|a| match a.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            _ => None,
        });
        tokio::spawn(crate::portmap::PortMap::open(lan_ip, et_port, public_ip))
    });
    // The punch socket too: the trackers give members its address, and the
    // hello sent there must get in even where hole punching fails.
    let use_trackers = std::env::var_os("UC_NO_TRACKER").is_none() && public_punch.is_some();
    let punch_map = (cfg.port_map && use_trackers).then(|| {
        let public_ip = public_punch.and_then(|a| match a.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            _ => None,
        });
        let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        tokio::spawn(crate::portmap::PortMap::open(lan_ip, port, public_ip))
    });

    // EasyTier starts without waiting for the presence slots: the virtual IP
    // comes from our id (a rare clash is reported below).
    stage("starting");
    let vip = cfg.fixed_ip.unwrap_or_else(|| pick_vip(&ident.id, cfg.subnet, &[]));
    status.send_modify(|s| s.my_vip = Some(vip));

    // Without adapter nothing reaches Windows' firewall (EasyTier hands
    // connections to local ports itself, restricted to the chat port).
    let _firewall = if cfg.game {
        let subnet_cidr = format!("10.{}.0.0/16", vip.octets()[1]);
        match FirewallRule::allow_subnet(&subnet_cidr) {
            Ok(r) => Some(r),
            Err(e) => {
                ev(format!("Pare-feu non configuré : {e:#}"));
                None
            }
        }
    } else {
        None
    };

    let socks5_port = easytier::free_port()?;
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
    let chat = Chat::start(room.chat_key(), &room.id, ident.clone(), banned.clone(), cfg.profile.clone(), socks5_port, chat_tx, cfg.data_dir.clone(), cfg.game && !easytier::test_no_tun()).await?;
    let _stop_chat = OnDrop({
        let c = chat.clone();
        move || c.stop()
    });
    let mut chat_profiles: HashMap<String, (String, String)> = HashMap::new();

    let et_dir = easytier::find_dir()?;
    // "<name>~<id>": peers learn our id from EasyTier alone (see below).
    let et_hostname = format!("{}~{}", my_name(), ident.id);
    let log_dir = cfg.data_dir.join("logs");
    let et_cfg = CoreConfig {
        network_name: &room.network_name,
        network_secret: &room.network_secret,
        ipv4_cidr: format!("{vip}/16"),
        hostname: &et_hostname,
        dev_name: format!("UC-{}", &room.id[..6.min(room.id.len())]),
        tun: cfg.game,
        open_tcp_ports: vec![chat.port()],
        socks5_port,
        machine_id: ident.machine_uuid(),
        listen_port: et_port,
        rpc_port: easytier::free_port()?,
        relays: &cfg.relays,
        log_dir: &log_dir,
    };
    let mut et = EasyTier::spawn(&et_dir, &et_cfg)?;
    // Restart times of EasyTier (it is restarted if it stops).
    let mut et_restarts: Vec<Instant> = vec![];
    ev(format!(
        "EasyTier démarré : IP virtuelle {vip}, port {et_port}, {}",
        if cfg.game { "mode jeu (carte réseau)" } else { "chat seulement (sans carte réseau)" }
    ));
    stage("searching");
    mark(&status, t0, &mut marks, "et", "EasyTier lancé".into());
    // Game network test: needs the virtual adapter.
    // Game announcements sent to 255.255.255.255 through any adapter: into
    // the salon (needs the virtual adapter and admin rights).
    let _bcast = if cfg.game && !easytier::test_no_tun() {
        match crate::bcast::BroadcastRelay::start(vip) {
            Ok(r) => Some(r),
            Err(e) => {
                ev(format!("Relais des annonces de jeux indisponible : {e:#}"));
                None
            }
        }
    } else {
        None
    };
    let (game_net, game_net_error) = if cfg.game && !easytier::test_no_tun() {
        let port = crate::gametest::port_for(&room.id);
        match crate::gametest::GameNet::start(Ipv4Addr::UNSPECIFIED, port, &room.id, &ident.id, vip).await {
            Ok(g) => {
                // Named in our answers to others' tests too.
                g.set_name(&my_name());
                (Some(g), String::new())
            }
            Err(e) => {
                let msg = format!("port UDP {port} indisponible : {e:#}");
                ev(format!("Test du réseau de jeu : {msg}"));
                (None, msg)
            }
        }
    } else {
        (None, "seulement en mode jeu, avec la carte réseau".to_string())
    };
    let _stop_game_net = OnDrop({
        let g = game_net.clone();
        move || {
            if let Some(g) = &g {
                g.stop()
            }
        }
    });
    let searching_since = Instant::now();

    let (punch_tx, mut punch_rx) = mpsc::unbounded_channel();
    let puncher = Puncher::new(sock, &ident.id, lan_ip, et_port, cfg.use_lan, punch_tx);
    let _stop_punch = OnDrop({
        let p = puncher.clone();
        move || p.stop()
    });
    if !cfg.use_lan {
        ev("Mode --no-lan : passage forcé par l'adresse publique".into());
    }

    let public_ip = disco.public_ipv4().await.or(public_punch.and_then(|a| match a.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        _ => None,
    }));
    // Port opening is waited for a moment only: a box that never answers
    // takes about 5 s to give up (NAT-PMP retries, then UPnP), and nobody
    // could find us meanwhile. Our record goes out now and is published
    // again if the port opens later (see the session loop).
    let mut port_map_task = port_map;
    let mut port_map = None;
    if let Some(task) = port_map_task.as_mut() {
        if let Ok(r) = tokio::time::timeout(PORTMAP_WAIT, task).await {
            port_map_task = None;
            port_map = port_map_done(r, &ev, &status, t0, &mut marks);
        }
    }
    // Reachable from outside: the opened port, otherwise our public IP with
    // EasyTier's port (works if it is forwarded by hand); and the LAN one.
    // Reachable members are joined at once, without hole punching.
    let direct_for = |pm: Option<&crate::portmap::PortMap>| -> Vec<String> {
        let public = match pm {
            Some(pm) => Some(format!("udp://{}", pm.external)),
            None => public_ip.map(|ip| format!("udp://{ip}:{et_port}")),
        };
        public.into_iter().chain(cfg.use_lan.then(|| format!("udp://{lan_ip}:{et_port}"))).collect()
    };
    let mut my_record = PeerRecord {
        id: ident.id.clone(),
        name: my_name(),
        avatar: profile.borrow().avatar.clone(),
        vip,
        ts: 0,
        punch: public_punch,
        punch_lan: cfg.use_lan.then(|| puncher.lan_addr()),
        direct: direct_for(port_map.as_ref()),
        chat_port: chat.port(),
    };
    // The DHT task publishes the latest one.
    let (direct_tx, direct_rx) = watch::channel(my_record.direct.clone());

    // Members subscribed to the salon on Nostr answer at once.
    if let Some(rv) = &rendezvous {
        rv.announce(&my_record);
    }
    // UDP trackers (see `tracker`): the punch socket's STUN address, which
    // is also the opened one when the box maps the same port (it usually
    // does). UC_NO_TRACKER=1 compares without them.
    puncher.set_intro(room.clone(), &my_record);
    let mut punch_map_task = punch_map;
    let mut punch_map = None;
    let (tr_tx, mut tr_rx) = mpsc::unbounded_channel();
    let trackers = public_punch.filter(|_| use_trackers).map(|a| {
        ev(format!("Trackers : annonce de {a}"));
        crate::tracker::Trackers::start(room.tracker_hash(), a, tr_tx.clone())
    });

    // Discovery task: read the others (each one handed over as soon as it is
    // found), publish our record, repeat.
    let (disc_tx, mut disc_rx) = mpsc::unbounded_channel::<Disc>();
    let pushed_tx = disc_tx.clone();
    // Members met before are tried right away, before any DHT answer.
    let known = salon_cache.members(&ident.id);
    if !known.is_empty() {
        ev(format!("{} membre(s) connu(s) essayé(s) directement", known.len()));
    }
    for r in known {
        let _ = disc_tx.send(Disc::Cached(r));
    }
    let poke = Arc::new(tokio::sync::Notify::new());
    let disc_task = {
        let disco = disco.clone();
        let mut rec = my_record.clone();
        let my_id = ident.id.clone();
        let mut profile = profile.clone();
        let mut direct_rx = direct_rx;
        let poke = poke.clone();
        tokio::spawn(async move {
            let mut round = 0u32;
            loop {
                let read_at = Instant::now();
                let slots = disco
                    .read_slots(|r| {
                        if discovery::is_other(r, &my_id) {
                            let _ = disc_tx.send(Disc::Found(r.clone()));
                        }
                    })
                    .await;
                if disc_tx.send(Disc::All(discovery::others(&slots, &my_id), disco.my_slot())).is_err() {
                    return;
                }
                {
                    let p = profile.borrow_and_update();
                    let n = profile_name(&p);
                    if !n.is_empty() {
                        rec.name = n;
                    }
                    rec.avatar = p.avatar.clone();
                }
                rec.direct = direct_rx.borrow_and_update().clone();
                rec.ts = discovery::now_secs();
                match disco.publish(&rec, &slots).await {
                    Ok(slot) => {
                        let _ = disc_tx.send(Disc::Published(slot));
                    }
                    Err(e) => tracing::warn!("publication DHT : {e:#}"),
                }
                round += 1;
                let wait = if round < 6 { DISCOVERY_FAST } else { DISCOVERY_SLOW };
                // A profile change (or a port opened late) is published right
                // away; a newcomer probing us is looked up right away.
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    r = profile.changed() => if r.is_err() { return },
                    r = direct_rx.changed() => if r.is_err() { return },
                    _ = poke.notified() => {
                        let min = read_at + DISCOVERY_POKE_MIN;
                        tokio::time::sleep_until(min.into()).await;
                    }
                }
            }
        })
    };

    let _stop_disc = OnDrop({
        let h = disc_task.abort_handle();
        move || h.abort()
    });
    let mut peers: HashMap<String, PeerState> = HashMap::new();
    let mut poked: HashMap<String, Instant> = HashMap::new();
    // Member id -> last record received through Nostr.
    let mut pushed_at: HashMap<String, Instant> = HashMap::new();
    // Member id -> id of the member relaying our traffic to it (EasyTier routes).
    let mut relayed_by: HashMap<String, String> = HashMap::new();
    let mut route_tick: u64 = 0;
    // Member id -> (name, path label): logs path changes.
    let mut paths: HashMap<String, (String, String)> = HashMap::new();
    // Member ids EasyTier is linked with (from its peer list).
    let mut et_linked: HashSet<String> = HashSet::new();
    let (net_tx, mut net_changed) = oneshot::channel();
    let net_task = tokio::spawn(watch_network(lan_ip, public_punch.map(|a| a.ip()), net_tx));
    let _stop_net = OnDrop({
        let h = net_task.abort_handle();
        move || h.abort()
    });
    let mut net_done = false;
    let mut exit = Exit::Stopped;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = &mut *shutdown => break,
            // A finished oneshot must not be polled again.
            r = &mut net_changed, if !net_done => {
                net_done = true;
                if let Ok(why) = r {
                    exit = Exit::NetworkChanged(why);
                    break;
                }
            }
            Ok(()) = profile.changed() => {
                let n = my_name();
                status.send_modify(|s| s.my_name = n);
            }
            Some(c) = ctl_rx.recv() => match c {
                Ctl::Say(text) => chat.say(&text),
                Ctl::WantImage(hash) => chat.request_image(&hash),
                Ctl::SalonChanged => chat.notify_salon_changed(),
                Ctl::SetBanned(b) => {
                    *banned = b.clone();
                    chat.set_banned(b);
                }
                Ctl::GameTest => match &game_net {
                    Some(g) => {
                        g.set_name(&my_name());
                        g.set_rivals(crate::system::broadcast_rivals(&et_cfg.dev_name));
                        let players = status
                            .borrow()
                            .members
                            .iter()
                            .filter(|m| m.path != "vous" && !m.id.is_empty())
                            .filter_map(|m| Some((m.id.clone(), m.name.clone(), m.vip.parse().ok()?)))
                            .collect();
                        g.run(players);
                    }
                    None => {
                        let error = Some(game_net_error.clone());
                        status.send_modify(|s| s.game_test = Some(crate::gametest::GameTest { error, ..Default::default() }));
                    }
                },
            },
            Some(e) = chat_rx.recv() => match e {
                ChatEvent::Line(line) => {
                    let _ = cfg.events_out.send(Event::Chat { salon_id: room.id.clone(), line });
                }
                ChatEvent::Profile { id, name, avatar } => {
                    chat_profiles.insert(id, (name, avatar));
                }
                ChatEvent::ImageReady(hash) => {
                    let _ = cfg.events_out.send(Event::ImageReady { hash });
                }
                ChatEvent::Member(member) => {
                    let _ = cfg.events_out.send(Event::Member { salon_id: room.id.clone(), member });
                }
                ChatEvent::SalonChanged => {
                    let _ = cfg.events_out.send(Event::SalonChanged { salon_id: room.id.clone() });
                }
            },
            Some(e) = rv_rx.recv() => match e {
                crate::nostr::RvEvent::Connected(relay) => {
                    mark(&status, t0, &mut marks, "nostr", format!("Nostr prêt ({})", relay.trim_start_matches("wss://")));
                }
                crate::nostr::RvEvent::Record(r, relay) => {
                    let _ = pushed_tx.send(Disc::Pushed(r, relay.trim_start_matches("wss://").to_string()));
                }
            },
            Some(e) = tr_rx.recv() => match e {
                crate::tracker::TrEvent::Ready(host) => {
                    mark(&status, t0, &mut marks, "tracker", format!("tracker prêt ({host})"));
                }
                crate::tracker::TrEvent::Peer(addr, host) => {
                    // Known members' addresses come back too: not introduced.
                    if puncher.introduce(addr) {
                        tracing::info!("tracker {host} : adresse inconnue {addr}, présentation envoyée");
                    }
                }
            },
            Some(r) = finished(&mut port_map_task) => {
                port_map = port_map_done(r, &ev, &status, t0, &mut marks);
                if port_map.is_some() {
                    my_record.direct = direct_for(port_map.as_ref());
                    let _ = direct_tx.send(my_record.direct.clone());
                    if let Some(rv) = &rendezvous {
                        rv.announce(&my_record);
                    }
                    puncher.set_intro(room.clone(), &my_record);
                }
            }
            Some(r) = finished(&mut punch_map_task) => match r {
                Ok(Ok(pm)) => {
                    tracing::info!("port de perçage ouvert sur la box ({}) : {}", pm.method, pm.external);
                    punch_map = Some(pm);
                }
                Ok(Err(e)) => tracing::info!("ouverture du port de perçage impossible : {e:#}"),
                Err(_) => {}
            },
            Some(d) = disc_rx.recv() => {
                let (recs, full) = match d {
                    Disc::Published(slot) => {
                        status.send_modify(|s| s.dht_slot = Some(slot));
                        mark(&status, t0, &mut marks, "published", "notre fiche publiée dans la DHT".into());
                        continue;
                    }
                    Disc::Cached(r) => (vec![r], false),
                    Disc::Found(r) => {
                        mark(&status, t0, &mut marks, "found", format!("premier membre trouvé dans la DHT ({})", r.name));
                        (vec![r], false)
                    }
                    Disc::Pushed(r, relay) => {
                        mark(&status, t0, &mut marks, "pushed", format!("premier membre trouvé via Nostr ({}, {relay})", r.name));
                        pushed_at.insert(r.id.clone(), Instant::now());
                        (vec![r], false)
                    }
                    Disc::Introduced(r, from) => {
                        mark(&status, t0, &mut marks, "introduced", format!("premier membre trouvé via les trackers ({}, {from})", r.name));
                        pushed_at.insert(r.id.clone(), Instant::now());
                        (vec![r], false)
                    }
                    Disc::All(recs, slot) => {
                        status.send_modify(|s| s.dht_slot = slot);
                        mark(&status, t0, &mut marks, "slots", format!("fiches de présence lues ({} membre(s))", recs.len()));
                        salon_cache.remember(&recs);
                        salon_cache.save(&cfg.data_dir, &room.id);
                        (recs, true)
                    }
                };
                let ids: HashSet<String> = recs.iter().map(|r| r.id.clone()).collect();
                for r in recs {
                    // An older record of a member we know (its previous
                    // session, still in another slot) must not replace the
                    // current address. Only while ours is fresh: if that
                    // member's clock went back, its new records win within
                    // STALE_SECS instead of never.
                    let newer_known = peers.get(&r.id).is_some_and(|p| {
                        p.rec.ts > r.ts && discovery::now_secs().saturating_sub(p.rec.ts) <= discovery::STALE_SECS
                    });
                    if newer_known {
                        continue;
                    }
                    if r.vip == vip && r.id < ident.id {
                        ev(format!("Conflit d'IP avec {} : relancez avec --ip", r.name));
                    }
                    // E.g. our LAN address changed: this member is retried at
                    // the next read instead of ending the whole session.
                    if let Err(e) = puncher.upsert_peer(&r, &ident.id).await {
                        tracing::warn!("perçage : {} ignoré pour l'instant : {e:#}", r.name);
                        continue;
                    }
                    let new = !peers.contains_key(&r.id);
                    if new {
                        ev(format!("Découvert : {} ({})", r.name, r.vip));
                        peers.insert(r.id.clone(), PeerState {
                            rec: r.clone(), wanted: None, connector: None,
                            connector_error: None, direct: vec![],
                        });
                    }
                    let p = peers.get_mut(&r.id).unwrap();
                    p.rec = r;
                    add_direct(&et, p).await;
                }
                // Partial results only add members; a full read also removes.
                if !full {
                    continue;
                }
                // Still linked by EasyTier = still there (e.g. a remembered
                // member whose DHT record is missing).
                let recently_pushed = |k: &String| pushed_at.get(k).is_some_and(|t| t.elapsed() < PUSHED_KEEP);
                let gone: Vec<String> = peers
                    .keys()
                    .filter(|k| !ids.contains(*k) && !et_linked.contains(*k) && !recently_pushed(k))
                    .cloned()
                    .collect();
                for id in gone {
                    let p = peers.remove(&id).unwrap();
                    puncher.remove_peer(&id);
                    remove_links(&et, &p).await;
                    ev(format!("{} a quitté le salon", p.rec.name));
                }
            }
            Some(pe) = punch_rx.recv() => {
                let (id, remote, bridge, dialer) = match pe {
                    PunchEvent::Established { id, remote, bridge, dialer } => (id, remote, bridge, dialer),
                    // A newcomer is probing us: look its record up now.
                    PunchEvent::Unknown { id } => {
                        if poked.get(&id).is_none_or(|t: &Instant| t.elapsed() > POKE_PER_PEER) {
                            poked.insert(id, Instant::now());
                            poke.notify_one();
                        }
                        continue;
                    }
                    PunchEvent::Record { rec, from } => {
                        let _ = pushed_tx.send(Disc::Introduced(rec, from));
                        continue;
                    }
                };
                let Some(p) = peers.get_mut(&id) else { continue };
                ev(format!("NAT percé avec {} via {remote}", p.rec.name));
                mark(&status, t0, &mut marks, "punch", format!("premier NAT percé ({})", p.rec.name));
                if dialer {
                    p.wanted = Some(format!("udp://{bridge}"));
                    if let Some(msg) = sync_connector(&et, p).await {
                        ev(msg);
                    }
                }
            }
            _ = tick.tick() => {
                if searching_since.elapsed() > SEARCH_PHASE {
                    stage("ready");
                }
                if et.exited() {
                    let cause = easytier::crash_cause(&log_dir);
                    et_restarts.retain(|t| t.elapsed() < ET_RESTART_WINDOW);
                    if et_restarts.len() >= ET_MAX_RESTARTS {
                        anyhow::bail!(
                            "easytier-core s'arrête sans cesse ({} fois en 5 min){}",
                            et_restarts.len() + 1,
                            cause.map(|c| format!(" : {c}")).unwrap_or_default()
                        );
                    }
                    ev(format!(
                        "EasyTier s'est arrêté, redémarrage{}",
                        cause.map(|c| format!(" (cause : {c})")).unwrap_or_default()
                    ));
                    et_restarts.push(Instant::now());
                    et = EasyTier::spawn(&et_dir, &et_cfg)?;
                    // Connectors are added again once its API answers.
                    for p in peers.values_mut() {
                        p.connector = None;
                        p.connector_error = None;
                        p.direct.clear();
                    }
                    continue;
                }
                for p in peers.values_mut() {
                    if let Some(msg) = sync_connector(&et, p).await {
                        ev(msg);
                    }
                    add_direct(&et, p).await;
                }
                // A member's path went silent: it probably restarted from a
                // new address. Look its new record up quickly (every 3 s).
                let dead = peers.values().any(|p| {
                    puncher.path(&p.rec.id).is_some_and(|(_, age)| age > punch::PATH_DEAD_AFTER && age < Duration::from_secs(90))
                });
                if dead {
                    poke.notify_one();
                }
                // easytier-cli costs a process launch: every second while the
                // session starts (quick feedback), then every 2 s.
                route_tick += 1;
                if t0.elapsed() > CLI_FAST_PHASE && route_tick % 2 == 0 {
                    continue;
                }
                // Who relays our traffic to whom (every ~6 s: one more CLI call).
                if route_tick % 6 == 1 {
                    if let Ok(routes) = et.routes().await {
                        let id_of = |h: &str| h.rsplit_once('~').map(|(_, id)| id.to_string());
                        relayed_by = routes
                            .iter()
                            .filter(|r| r.path_len > 1)
                            .filter_map(|r| Some((id_of(&r.hostname)?, id_of(&r.next_hop_hostname)?)))
                            .collect();
                    }
                }
                if let Ok(list) = et.peers().await {
                    et_linked = list
                        .iter()
                        .filter(|e| e.cost != "Local")
                        .filter_map(|e| e.hostname.rsplit_once('~').map(|(_, id)| id.to_string()))
                        .collect();
                    for e in list.iter().filter(|e| e.cost != "Local") {
                        let name = e.hostname.rsplit_once('~').map_or(e.hostname.as_str(), |(n, _)| n);
                        if e.cost == "p2p" {
                            mark(&status, t0, &mut marks, "et_p2p", format!("premier lien EasyTier direct ({name})"));
                        } else {
                            mark(&status, t0, &mut marks, "et_relay", format!("premier lien EasyTier par relais ({name})"));
                        }
                    }
                    if !chat.linked().is_empty() {
                        mark(&status, t0, &mut marks, "chat", "premier lien du chat".into());
                    }
                    // Open the chat once EasyTier has a route to the member
                    // (dialing earlier only waits for timeouts), without
                    // waiting for the member's DHT record.
                    for e in list.iter().filter(|e| e.cost != "Local") {
                        let (Some((_, id)), Ok(vip)) = (e.hostname.rsplit_once('~'), e.ipv4.parse()) else { continue };
                        let port = peers.get(id).map(|p| p.rec.chat_port).filter(|p| *p != 0)
                            .unwrap_or_else(|| crate::chat::port_for(&room.id));
                        chat.ensure_peer(id, vip, port);
                    }
                    let me = cfg.profile.borrow().clone();
                    let linked = chat.linked();
                    let members = list.into_iter().map(|e| {
                        let peer = peers.values().find(|p| p.rec.vip.to_string() == e.ipv4);
                        let punched = peer
                            .and_then(|p| puncher.path(&p.rec.id))
                            .is_some_and(|(_, age)| age < Duration::from_secs(20));
                        let path = match e.cost.as_str() {
                            "Local" => "vous".to_string(),
                            "p2p" if punched => format!("direct (NAT percé, {})", e.tunnel_proto),
                            "p2p" => format!("direct ({})", e.tunnel_proto),
                            other => format!("relais [{other}]"),
                        };
                        // Live name/avatar from the DHT record; EasyTier's hostname
                        // is only set when the session starts.
                        // The chat link carries profile changes instantly.
                        let (name, avatar) = match (e.cost.as_str(), peer) {
                            ("Local", _) => (my_name(), me.avatar.clone()),
                            (_, Some(p)) => chat_profiles
                                .get(&p.rec.id)
                                .cloned()
                                .unwrap_or((p.rec.name.clone(), p.rec.avatar.clone())),
                            _ => (e.hostname.rsplit_once('~').map(|(n, _)| n.to_string()).unwrap_or_else(|| e.hostname.clone()), String::new()),
                        };
                        let name: String = name.chars().take(32).collect();
                        let avatar: String = avatar.chars().take(72).collect();
                        let id = match (e.cost.as_str(), peer) {
                            ("Local", _) => ident.id.clone(),
                            (_, Some(p)) => p.rec.id.clone(),
                            _ => e.hostname.rsplit_once('~').map(|(_, id)| id.to_string()).unwrap_or_default(),
                        };
                        let chat_up = e.cost == "Local" || linked.contains(&id);
                        let relay_via = if e.cost == "p2p" || e.cost == "Local" {
                            String::new()
                        } else {
                            relayed_by.get(&id).cloned().unwrap_or_default()
                        };
                        Member { id, chat: chat_up, name, avatar, vip: e.ipv4, latency_ms: e.lat_ms, loss: e.loss_rate, path, nat: e.nat_type, relay_via, ping: None }
                    }).collect::<Vec<_>>();
                    let members = without_ghosts(members, &ident.id);
                    // Relaying member: by id above, shown by its live name.
                    let names: HashMap<String, String> = members.iter().map(|m| (m.id.clone(), m.name.clone())).collect();
                    let mut members: Vec<Member> = members
                        .into_iter()
                        .map(|mut m| {
                            if let Some(n) = names.get(&m.relay_via) {
                                m.relay_via = n.clone();
                            }
                            m
                        })
                        .collect();
                    if let Some(g) = &game_net {
                        g.set_ping_targets(
                            members
                                .iter()
                                .filter(|m| m.path != "vous" && !m.id.is_empty())
                                .filter_map(|m| Some((m.id.clone(), m.vip.parse().ok()?)))
                                .collect(),
                        );
                        let mut stats = g.ping_stats();
                        for m in &mut members {
                            m.ping = stats.remove(&m.id);
                        }
                    }
                    let mut changes = vec![];
                    for m in members.iter().filter(|m| m.path != "vous" && !m.id.is_empty()) {
                        let label = if m.relay_via.is_empty() { m.path.clone() } else { format!("{} par {}", m.path, m.relay_via) };
                        match paths.insert(m.id.clone(), (m.name.clone(), label.clone())) {
                            Some((_, old)) if old == label => {}
                            Some((_, old)) => changes.push(format!("{} : {old} → {label}", m.name)),
                            None => changes.push(format!("{} : {label}", m.name)),
                        }
                    }
                    paths.retain(|id, (name, _)| {
                        let here = members.iter().any(|m| &m.id == id);
                        if !here {
                            changes.push(format!("{name} : plus de lien"));
                        }
                        here
                    });
                    let game_test = game_net.as_ref().and_then(|g| g.snapshot()).map(|mut t| {
                        t.relayed = _bcast.as_ref().map(|r| r.relayed.load(std::sync::atomic::Ordering::Relaxed));
                        t
                    });
                    status.send_modify(|s| {
                        s.members = members;
                        if game_test.is_some() {
                            s.game_test = game_test;
                        }
                        for c in changes {
                            tracing::info!("chemin : {c}");
                            s.path_log.push(format!("{} [+{}s] {c}", local_hms(), t0.elapsed().as_secs()));
                        }
                        let n = s.path_log.len();
                        if n > PATH_LOG_MAX {
                            s.path_log.drain(..n - PATH_LOG_MAX);
                        }
                    });
                }
            }
        }
    }

    disc_task.abort();
    dht_check.abort();
    if let Some(g) = &game_net {
        g.stop();
    }
    et.stop().await;
    if let Some(t) = trackers {
        t.stop().await;
    }
    if let Some(pm) = port_map {
        pm.close().await;
    }
    if let Some(pm) = punch_map {
        pm.close().await;
    }
    // Still trying: a mapping made after the end would stay open.
    for t in [port_map_task, punch_map_task].into_iter().flatten() {
        t.abort();
    }
    ev("Session arrêtée".into());
    Ok(exit)
}

/// A member that just restarted (or we did) shows twice for about 20 s:
/// EasyTier keeps the previous node until it times out. Only the "vous"
/// entry stays for us, and each member keeps its best entry.
fn without_ghosts(members: Vec<Member>, my_id: &str) -> Vec<Member> {
    let num = |s: &str| s.trim_end_matches('%').trim().parse::<f32>().unwrap_or(f32::MAX);
    let worse = |a: &Member, b: &Member| (num(&a.loss), num(&a.latency_ms)) > (num(&b.loss), num(&b.latency_ms));
    let mut best: HashMap<&str, usize> = HashMap::new();
    for (i, m) in members.iter().enumerate() {
        if m.id.is_empty() || (m.id == my_id && m.path != "vous") {
            continue;
        }
        match best.get(m.id.as_str()) {
            Some(&j) if members[j].path == "vous" || !worse(&members[j], m) => {}
            _ => {
                best.insert(&m.id, i);
            }
        }
    }
    let keep: HashSet<usize> = best.into_values().collect();
    members.into_iter().enumerate().filter(|(i, m)| m.id.is_empty() || keep.contains(i)).map(|(_, m)| m).collect()
}

fn profile_name(p: &Profile) -> String {
    p.name.trim().chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, path: &str, loss: &str, lat: &str) -> Member {
        Member { id: id.into(), path: path.into(), loss: loss.into(), latency_ms: lat.into(), ..Default::default() }
    }

    #[test]
    fn restarted_members_show_once() {
        let list = vec![
            m("me", "vous", "-", "-"),
            m("bob", "direct", "50.0%", "0.3"),
            m("me", "relais [relay(2)]", "0.0%", "1000"),
            m("bob", "direct", "0.0%", "0.2"),
            m("", "relais", "0.0%", "5"),
        ];
        let out = without_ghosts(list, "me");
        let got: Vec<(&str, &str)> = out.iter().map(|m| (m.id.as_str(), m.loss.as_str())).collect();
        assert_eq!(got, [("me", "-"), ("bob", "0.0%"), ("", "0.0%")]);
    }
}
