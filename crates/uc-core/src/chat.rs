//! Salon text chat: a full mesh of TCP connections between members, carried
//! by the salon's EasyTier network (through its SOCKS5 proxy, so it works
//! with or without a virtual adapter). Nothing is stored: no history.
//!
//! The lower id dials. Both sides open with an HMAC (salon key) so a peer of
//! another salon reaching this port cannot join the conversation.

use crate::identity::{self, Identity};
use crate::images;
use crate::salons::Member;
use crate::session::Profile;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::Instant;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

/// Fits an image line (hex-encoded `images::MAX_IMAGE`).
const MAX_LINE: u64 = 128 * 1024;
pub const MAX_TEXT: usize = 2000;
/// "emoji|#rrggbb" or "img:<sha256>".
const MAX_AVATAR: usize = 72;
/// Don't ask peers for the same missing image more often than this.
const IMAGE_RETRY: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatLine {
    /// Sender id ("" for system lines).
    pub from: String,
    pub name: String,
    pub avatar: String,
    pub text: String,
    /// Unix time, seconds.
    pub ts: u64,
    pub mine: bool,
    pub system: bool,
}

pub enum ChatEvent {
    Line(ChatLine),
    /// A member's live profile (instant, unlike the DHT record).
    Profile { id: String, name: String, avatar: String },
    /// An image (custom avatar) arrived in the local cache.
    ImageReady(String),
    /// A member says the owner changed the salon (name, avatar...): refetch.
    SalonChanged,
    /// A member proved its identity (signed hello): for the salon roster.
    Member(Member),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Wire {
    Hello {
        id: String,
        name: String,
        avatar: String,
        /// HMAC with the salon key: proves membership.
        mac: String,
        /// Ed25519 identity key + X25519 key, signed: proves identity.
        pk: String,
        xpk: String,
        sig: String,
    },
    Msg {
        text: String,
        /// Random id: a message replayed on a new link is shown once.
        #[serde(default)]
        id: String,
        /// Send time (unix seconds), kept when replayed.
        #[serde(default)]
        ts: u64,
    },
    Profile { name: String, avatar: String },
    GetImage { hash: String },
    /// `data` is hex encoded.
    Image { hash: String, data: String },
    SalonChanged,
}

struct Conn {
    tx: mpsc::UnboundedSender<Wire>,
    name: String,
    avatar: String,
    epoch: u64,
}

pub struct Chat {
    key: [u8; 32],
    salon_id: String,
    me: Identity,
    my_id: String,
    banned: Mutex<HashSet<String>>,
    profile: watch::Receiver<Profile>,
    socks: SocketAddr,
    port: u16,
    conns: Mutex<HashMap<String, Conn>>,
    dialing: Mutex<HashSet<String>>,
    /// Higher-id side: since when we wait for the member to dial us.
    waiting: Mutex<HashMap<String, Instant>>,
    next_epoch: Mutex<u64>,
    events: mpsc::UnboundedSender<ChatEvent>,
    data_dir: PathBuf,
    /// Game mode (virtual adapter): try the direct path first.
    direct_first: bool,
    dial_failed: Mutex<HashSet<String>>,
    asked: Mutex<HashMap<String, Instant>>,
    /// Our recent messages, replayed to members whose link comes up later
    /// (messages written before the chat reached them are not lost).
    sent: Mutex<VecDeque<(String, u64, String)>>,
    /// "<sender>/<message id>" already shown.
    seen: Mutex<HashSet<String>>,
    /// Set by `stop`: every link ends.
    stopped: watch::Sender<bool>,
    /// Accept and profile loops (they hold the listener, hence the port).
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

/// The higher id normally waits to be dialed; it dials itself after this.
const DIAL_ANYWAY_AFTER: Duration = Duration::from_secs(5);

/// Replay window for messages written before a member's link was up.
const REPLAY_SECS: u64 = 10 * 60;
const REPLAY_MAX: usize = 50;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect::<String>().trim().to_string()
}

impl Chat {
    pub async fn start(
        key: [u8; 32],
        salon_id: &str,
        me: Identity,
        banned: Vec<String>,
        profile: watch::Receiver<Profile>,
        socks_port: u16,
        events: mpsc::UnboundedSender<ChatEvent>,
        data_dir: PathBuf,
        direct_first: bool,
    ) -> Result<Arc<Self>> {
        // Same port for every member of the salon, so peers can reach us as
        // soon as the network links us, without waiting for the DHT record.
        let listener = match TcpListener::bind(("0.0.0.0", port_for(salon_id))).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind("0.0.0.0:0").await?,
        };
        let chat = Arc::new(Self {
            key,
            salon_id: salon_id.to_string(),
            my_id: me.id.clone(),
            me,
            banned: Mutex::new(banned.into_iter().collect()),
            profile,
            socks: SocketAddr::from(([127, 0, 0, 1], socks_port)),
            port: listener.local_addr()?.port(),
            conns: Default::default(),
            dialing: Default::default(),
            waiting: Default::default(),
            next_epoch: Mutex::new(0),
            events,
            data_dir,
            direct_first,
            dial_failed: Default::default(),
            asked: Default::default(),
            sent: Default::default(),
            seen: Default::default(),
            stopped: watch::channel(false).0,
            tasks: Default::default(),
        });
        let c = chat.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(c.clone().handle(stream, None));
            }
        });
        let profile = tokio::spawn(chat.clone().profile_loop());
        *chat.tasks.lock().unwrap() = vec![accept.abort_handle(), profile.abort_handle()];
        Ok(chat)
    }

    /// Ends the session's chat: frees the salon's chat port (a later session
    /// of the same salon needs it) and closes every link.
    pub fn stop(&self) {
        self.stopped.send_replace(true);
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        self.conns.lock().unwrap().clear();
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Members whose chat link is up.
    pub fn linked(&self) -> HashSet<String> {
        self.conns.lock().unwrap().keys().cloned().collect()
    }

    fn mac(&self, id: &str) -> String {
        let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key).unwrap();
        m.update(b"uc-chat-hello/");
        m.update(id.as_bytes());
        hex::encode(m.finalize().into_bytes())
    }

    fn hello_msg(&self, id: &str, xpk: &str) -> Vec<u8> {
        format!("uc-hello/{}/{id}/{xpk}", self.salon_id).into_bytes()
    }

    fn is_banned(&self, id: &str) -> bool {
        self.banned.lock().unwrap().contains(id)
    }

    /// Applies a new ban list; banned members are cut off at once.
    pub fn set_banned(&self, banned: Vec<String>) {
        let set: HashSet<String> = banned.into_iter().collect();
        let mut gone = vec![];
        self.conns.lock().unwrap().retain(|id, c| {
            let keep = !set.contains(id);
            if !keep {
                gone.push(c.name.clone());
            }
            keep
        });
        *self.banned.lock().unwrap() = set;
        for name in gone {
            self.system(format!("{name} a été banni du salon"));
        }
    }

    /// Called for every member EasyTier can reach; dials when we are the
    /// lower id, or when the other side has not dialed for a while (it may
    /// still hold a dead link to our previous session).
    pub fn ensure_peer(self: &Arc<Self>, id: &str, vip: Ipv4Addr, port: u16) {
        if port == 0 || id == self.my_id || self.is_banned(id) || self.conns.lock().unwrap().contains_key(id) {
            return;
        }
        if self.my_id.as_str() > id {
            let mut waiting = self.waiting.lock().unwrap();
            let since = *waiting.entry(id.to_string()).or_insert_with(Instant::now);
            if since.elapsed() < DIAL_ANYWAY_AFTER {
                return;
            }
        }
        if !self.dialing.lock().unwrap().insert(id.to_string()) {
            return;
        }
        let me = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            match me.dial(vip, port).await {
                Ok(stream) => {
                    tracing::info!("chat : liaison ouverte vers {id} ({vip}:{port})");
                    me.clone().handle(stream, Some(id.clone())).await
                }
                Err(e) => {
                    // Logged once per member, retried every couple of seconds.
                    if me.dial_failed.lock().unwrap().insert(id.clone()) {
                        tracing::info!("chat : connexion à {id} ({vip}:{port}) impossible pour l'instant : {e:#}");
                    }
                }
            }
            me.dialing.lock().unwrap().remove(&id);
        });
    }

    /// Directly (needs our virtual adapter) and through EasyTier's SOCKS5
    /// (works without adapter), each with its own timeout: one path hanging
    /// must not starve the other.
    async fn dial(&self, ip: Ipv4Addr, port: u16) -> Result<TcpStream> {
        let t = Duration::from_secs(5);
        let direct = || async move {
            let s = tokio::time::timeout(t, TcpStream::connect((ip, port))).await.context("délai dépassé (direct)")??;
            anyhow::Ok(s)
        };
        let socks = || async move {
            tokio::time::timeout(t, socks5_connect(self.socks, ip, port)).await.context("délai dépassé (socks5)")?
        };
        if self.direct_first {
            match direct().await {
                Ok(s) => Ok(s),
                Err(e) => socks().await.map_err(|e2| e2.context(format!("direct : {e:#}"))),
            }
        } else {
            match socks().await {
                Ok(s) => Ok(s),
                Err(e) => direct().await.map_err(|e2| e2.context(format!("socks5 : {e:#}"))),
            }
        }
    }

    pub fn say(&self, text: &str) {
        let text = clean(text, MAX_TEXT);
        if text.is_empty() {
            return;
        }
        let p = self.profile.borrow().clone();
        let (id, ts) = (hex::encode(rand::random::<[u8; 8]>()), now());
        let _ = self.events.send(ChatEvent::Line(ChatLine {
            from: self.my_id.clone(),
            name: p.name,
            avatar: p.avatar,
            text: text.clone(),
            ts,
            mine: true,
            system: false,
        }));
        {
            let mut sent = self.sent.lock().unwrap();
            sent.push_back((id.clone(), ts, text.clone()));
            while sent.len() > REPLAY_MAX || sent.front().is_some_and(|m| ts - m.1 > REPLAY_SECS) {
                sent.pop_front();
            }
        }
        for c in self.conns.lock().unwrap().values() {
            let _ = c.tx.send(Wire::Msg { text: text.clone(), id: id.clone(), ts });
        }
    }

    /// Asks connected members for an image we don't have (any of them may have it).
    pub fn request_image(&self, hash: &str) {
        let conns = self.conns.lock().unwrap();
        // Nobody to ask yet: don't burn the retry delay.
        if conns.is_empty() || images::has(&self.data_dir, hash) || !self.should_ask(hash) {
            return;
        }
        for c in conns.values() {
            let _ = c.tx.send(Wire::GetImage { hash: hash.to_string() });
        }
    }

    fn should_ask(&self, hash: &str) -> bool {
        let mut asked = self.asked.lock().unwrap();
        if asked.get(hash).is_some_and(|t| t.elapsed() < IMAGE_RETRY) {
            return false;
        }
        asked.insert(hash.to_string(), Instant::now());
        true
    }

    /// Fetches a member's custom avatar from that member if missing (it
    /// surely has it, so no retry delay applies).
    fn want_avatar(&self, tx: &mpsc::UnboundedSender<Wire>, avatar: &str) {
        if let Some(h) = images::avatar_hash(avatar) {
            if !images::has(&self.data_dir, h) {
                let _ = tx.send(Wire::GetImage { hash: h.to_string() });
            }
        }
    }

    /// Tells members to refetch the salon manifest (sent by the owner).
    pub fn notify_salon_changed(&self) {
        for c in self.conns.lock().unwrap().values() {
            let _ = c.tx.send(Wire::SalonChanged);
        }
    }

    fn system(&self, text: String) {
        let _ = self.events.send(ChatEvent::Line(ChatLine {
            from: String::new(),
            name: String::new(),
            avatar: String::new(),
            text,
            ts: now(),
            mine: false,
            system: true,
        }));
    }

    async fn profile_loop(self: Arc<Self>) {
        let mut rx = self.profile.clone();
        while rx.changed().await.is_ok() {
            let p = rx.borrow_and_update().clone();
            for c in self.conns.lock().unwrap().values() {
                let _ = c.tx.send(Wire::Profile { name: p.name.clone(), avatar: p.avatar.clone() });
            }
        }
    }

    async fn handle(self: Arc<Self>, stream: TcpStream, expected: Option<String>) {
        let _ = stream.set_nodelay(true);
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        let p = self.profile.borrow().clone();
        let xpk = self.me.x25519_public_hex();
        let hello = Wire::Hello {
            id: self.my_id.clone(),
            name: p.name,
            avatar: p.avatar,
            mac: self.mac(&self.my_id),
            pk: self.me.public_key_hex(),
            sig: self.me.sign_hex(&self.hello_msg(&self.my_id, &xpk)),
            xpk,
        };
        if write_line(&mut w, &hello).await.is_err() {
            return;
        }
        let Ok(Ok(Some(Wire::Hello { id, name, avatar, mac, pk, xpk, sig }))) =
            tokio::time::timeout(Duration::from_secs(10), read_line(&mut reader)).await
        else {
            return;
        };
        if mac != self.mac(&id) || id == self.my_id || expected.as_ref().is_some_and(|e| *e != id) {
            return;
        }
        // Identity proof: nobody can claim another member's id (and get its keys).
        if !identity::verify(&id, &pk, &self.hello_msg(&id, &xpk), &sig) || self.is_banned(&id) {
            return;
        }
        let name = clean(&name, 32);
        let _ = self.events.send(ChatEvent::Member(Member { id: id.clone(), pk, xpk, name: name.clone() }));
        let avatar = clean(&avatar, MAX_AVATAR);

        let (tx, mut rx) = mpsc::unbounded_channel::<Wire>();
        self.want_avatar(&tx, &avatar);
        let reply = tx.clone();
        // Messages we wrote before this link existed (the peer drops duplicates).
        for (mid, ts, text) in self.sent.lock().unwrap().iter() {
            if now().saturating_sub(*ts) <= REPLAY_SECS {
                let _ = tx.send(Wire::Msg { text: text.clone(), id: mid.clone(), ts: *ts });
            }
        }
        let epoch = {
            let mut g = self.next_epoch.lock().unwrap();
            *g += 1;
            *g
        };
        // A new link replaces any older one (e.g. to our previous session).
        let rejoined = self
            .conns
            .lock()
            .unwrap()
            .insert(id.clone(), Conn { tx, name: name.clone(), avatar: avatar.clone(), epoch })
            .is_some();
        self.waiting.lock().unwrap().remove(&id);
        let _ = self.events.send(ChatEvent::Profile { id: id.clone(), name: name.clone(), avatar });
        if !rejoined {
            self.system(format!("{name} a rejoint le salon"));
        }

        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if write_line(&mut w, &m).await.is_err() {
                    break;
                }
            }
        });
        let mut stopped = self.stopped.subscribe();
        loop {
            if *stopped.borrow() {
                break;
            }
            let m = tokio::select! {
                m = read_line(&mut reader) => m,
                _ = stopped.changed() => break,
            };
            let Ok(Some(m)) = m else { break };
            if self.is_banned(&id) {
                break;
            }
            match m {
                Wire::Msg { text, id: mid, ts } => {
                    let text = clean(&text, MAX_TEXT);
                    if text.is_empty() {
                        continue;
                    }
                    if !mid.is_empty() {
                        let mut seen = self.seen.lock().unwrap();
                        if seen.len() > 5000 {
                            seen.clear();
                        }
                        if !seen.insert(format!("{id}/{mid}")) {
                            continue;
                        }
                    }
                    // Replayed messages keep their send time (if plausible).
                    let ts = if ts > 0 && ts <= now() + 60 { ts } else { now() };
                    let (name, avatar) = self.peer_profile(&id);
                    let _ = self.events.send(ChatEvent::Line(ChatLine {
                        from: id.clone(),
                        name,
                        avatar,
                        text,
                        ts,
                        mine: false,
                        system: false,
                    }));
                }
                Wire::GetImage { hash } => {
                    if let Some(bytes) = images::load(&self.data_dir, &hash) {
                        let _ = reply.send(Wire::Image { hash, data: hex::encode(bytes) });
                    }
                }
                Wire::Image { hash, data } => {
                    let ok = hex::decode(&data)
                        .ok()
                        .is_some_and(|b| images::save_verified(&self.data_dir, &hash, &b).is_ok());
                    if ok {
                        let _ = self.events.send(ChatEvent::ImageReady(hash));
                    }
                }
                Wire::SalonChanged => {
                    let _ = self.events.send(ChatEvent::SalonChanged);
                }
                Wire::Profile { name, avatar } => {
                    let name = clean(&name, 32);
                    let avatar = clean(&avatar, MAX_AVATAR);
                    self.want_avatar(&reply, &avatar);
                    let old = {
                        let mut conns = self.conns.lock().unwrap();
                        conns.get_mut(&id).map(|c| {
                            c.avatar = avatar.clone();
                            std::mem::replace(&mut c.name, name.clone())
                        })
                    };
                    if let Some(old) = old.filter(|o| *o != name) {
                        self.system(format!("{old} s'appelle désormais {name}"));
                    }
                    let _ = self.events.send(ChatEvent::Profile { id: id.clone(), name, avatar });
                }
                Wire::Hello { .. } => {}
            }
        }
        writer.abort();
        let removed = {
            let mut conns = self.conns.lock().unwrap();
            match conns.get(&id) {
                Some(c) if c.epoch == epoch => conns.remove(&id),
                _ => None,
            }
        };
        if let Some(c) = removed {
            self.system(format!("{} a quitté le salon", c.name));
        }
    }

    fn peer_profile(&self, id: &str) -> (String, String) {
        self.conns
            .lock()
            .unwrap()
            .get(id)
            .map(|c| (c.name.clone(), c.avatar.clone()))
            .unwrap_or_default()
    }
}

async fn write_line(w: &mut (impl AsyncWriteExt + Unpin), m: &Wire) -> Result<()> {
    let mut line = serde_json::to_vec(m)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

async fn read_line(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Option<Wire>> {
    let mut buf = Vec::new();
    let n = (&mut *r).take(MAX_LINE).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        bail!("ligne trop longue");
    }
    Ok(Some(serde_json::from_slice(&buf)?))
}

async fn socks5_connect(proxy: SocketAddr, ip: Ipv4Addr, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect(proxy).await?;
    s.write_all(&[5, 1, 0]).await?;
    let mut b = [0u8; 2];
    s.read_exact(&mut b).await?;
    if b != [5, 0] {
        bail!("socks5 : authentification refusée");
    }
    let mut req = vec![5, 1, 0, 1];
    req.extend(ip.octets());
    req.extend(port.to_be_bytes());
    s.write_all(&req).await?;
    let mut rep = [0u8; 10];
    s.read_exact(&mut rep).await?;
    if rep[1] != 0 {
        bail!("socks5 : connexion refusée ({})", rep[1]);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stopped chat frees its port: the next session of the salon (game
    /// mode switch, new key) must get the salon's port again.
    #[tokio::test]
    async fn stop_frees_the_port() {
        let dir = std::env::temp_dir().join(format!("uc-chat-test-{}", std::process::id()));
        let me = Identity::load_or_create(&dir).unwrap();
        let (_profile_tx, profile) = watch::channel(Profile::default());
        let (events, _events_rx) = mpsc::unbounded_channel();
        let chat = Chat::start([1; 32], "salon-test-stop", me, vec![], profile, 1, events, dir.clone(), false).await.unwrap();
        let port = chat.port();
        assert!(TcpListener::bind(("0.0.0.0", port)).await.is_err());
        chat.stop();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(TcpListener::bind(("0.0.0.0", port)).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Chat port shared by all members of a salon (20000..=59999).
pub fn port_for(salon_id: &str) -> u16 {
    use sha2::Digest;
    let h = Sha256::digest(format!("uc-chat-port/{salon_id}").as_bytes());
    20000 + u16::from_be_bytes([h[0], h[1]]) % 40000
}