//! Fast rendezvous through public Nostr relays, on top of the DHT.
//!
//! The DHT is slow (seconds per read) and never tells us anything: members
//! find each other at their next read, at different moments, so they do
//! not start hole punching together. Here a member arriving in a salon
//! publishes its presence record on a few relays; members subscribed to the
//! salon's topic get it within ~0.4 s and answer with their own record.
//! Both sides then punch at the same time.
//!
//! Records are encrypted with the salon key, as in the DHT: relays see
//! nothing, and only members can write one. The Nostr key is thrown away
//! after the session (unrelated to our identity). If every relay fails,
//! discovery goes on through the DHT alone.

use crate::discovery::{self, PeerRecord};
use crate::room::Room;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use k256::schnorr::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

/// Relays that answered well from New Caledonia (two ISPs, 2026-10-29);
/// one is enough, the others are spares.
pub const RELAYS: &[&str] = &[
    "wss://relay.snort.social",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://nostr.mom",
    "wss://relay.damus.io",
];
/// Ephemeral range (20000-29999): relays forward these without storing them.
const KIND: u32 = 21_337;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A lost relay is tried again after this delay.
const RECONNECT: Duration = Duration::from_secs(30);
/// Keeps idle connections open (and finds dead ones).
const PING_EVERY: Duration = Duration::from_secs(30);
/// We answer the same member's hello at most this often.
const ANSWER_EVERY: Duration = Duration::from_secs(5);

/// What we send: our presence record, and whether the others should answer.
#[derive(Serialize, Deserialize)]
struct Msg {
    /// "hello" (just arrived: please answer) or "answer".
    t: String,
    rec: PeerRecord,
}

pub enum RvEvent {
    /// A relay is connected and subscribed (first one: worth a milestone).
    Connected(String),
    /// A member's fresh record, pushed by a relay.
    Record(PeerRecord, String),
}

struct Shared {
    room: Arc<Room>,
    topic: String,
    key: SigningKey,
    my_id: String,
    /// Our latest record (`None` until it is known).
    record: Mutex<Option<PeerRecord>>,
    /// Event ids already handled (the same event comes from every relay).
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
    answered: Mutex<HashMap<String, Instant>>,
    out: broadcast::Sender<String>,
    events: mpsc::UnboundedSender<RvEvent>,
}

pub struct Rendezvous {
    shared: Arc<Shared>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Rendezvous {
    /// Connects to the relays right away (records come later, see `announce`).
    pub fn start(room: Arc<Room>, my_id: &str, events: mpsc::UnboundedSender<RvEvent>) -> Self {
        let (out, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            topic: room.nostr_topic(),
            room,
            key: new_key(),
            my_id: my_id.to_string(),
            record: Mutex::new(None),
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
            answered: Mutex::new(HashMap::new()),
            out,
            events,
        });
        let tasks = RELAYS.iter().map(|url| tokio::spawn(relay_loop(shared.clone(), url))).collect();
        Self { shared, tasks }
    }

    /// Publishes our record ("hello": members answer with theirs). Relays
    /// connecting later get it too.
    pub fn announce(&self, rec: &PeerRecord) {
        *self.shared.record.lock().unwrap() = Some(rec.clone());
        if let Some(ev) = self.shared.event("hello") {
            let _ = self.shared.out.send(ev);
        }
    }
}

impl Shared {
    /// `["EVENT", …]` carrying our current record, freshly dated.
    fn event(&self, t: &str) -> Option<String> {
        let mut rec = self.record.lock().unwrap().clone()?;
        rec.ts = discovery::now_secs();
        let plain = serde_json::to_vec(&Msg { t: t.into(), rec }).ok()?;
        let ev = signed_event(&self.key, &self.topic, &hex::encode(self.room.encrypt(&plain)));
        Some(json!(["EVENT", ev]).to_string())
    }

    /// First time we see this event id (it arrives once per relay).
    fn first_sight(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if !seen.0.insert(id.to_string()) {
            return false;
        }
        seen.1.push_back(id.to_string());
        if seen.1.len() > 512 {
            let old = seen.1.pop_front().unwrap();
            seen.0.remove(&old);
        }
        true
    }

    fn on_event(&self, ev: &Value, relay: &str) {
        let Some(id) = ev["id"].as_str() else { return };
        if !self.first_sight(id) {
            return;
        }
        // Anyone can post under our topic: only what decrypts with the salon
        // key counts.
        let Some(msg) = ev["content"]
            .as_str()
            .and_then(|c| hex::decode(c).ok())
            .and_then(|b| self.room.decrypt(&b).ok())
            .and_then(|p| serde_json::from_slice::<Msg>(&p).ok())
        else {
            return;
        };
        if !discovery::is_other(&msg.rec, &self.my_id) {
            return;
        }
        if msg.t == "hello" {
            let due = {
                let mut answered = self.answered.lock().unwrap();
                let due = answered.get(&msg.rec.id).is_none_or(|t| t.elapsed() > ANSWER_EVERY);
                if due {
                    answered.insert(msg.rec.id.clone(), Instant::now());
                }
                due
            };
            if due {
                if let Some(ev) = self.event("answer") {
                    let _ = self.out.send(ev);
                }
            }
        }
        let _ = self.events.send(RvEvent::Record(msg.rec, relay.to_string()));
    }
}

/// One relay, reconnected whenever it drops.
async fn relay_loop(shared: Arc<Shared>, url: &'static str) {
    // Failures are logged once in a row (a dead relay is retried quietly).
    let mut quiet = false;
    loop {
        let mut worked = false;
        let r = relay_session(&shared, url, &mut worked).await;
        if worked {
            quiet = false;
        }
        if !quiet {
            let why = r.err().map_or("connexion fermée".into(), |e| format!("{e:#}"));
            tracing::info!("Nostr : relais {url} indisponible ({why}), nouvel essai toutes les {} s", RECONNECT.as_secs());
            quiet = true;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

/// `worked`: set once the relay accepted our subscription.
async fn relay_session(shared: &Shared, url: &str, worked: &mut bool) -> Result<()> {
    let t = Instant::now();
    // Subscribed before connecting: nothing published meanwhile is missed.
    let mut out = shared.out.subscribe();
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .context("délai dépassé")??;
    let (mut tx, mut rx) = ws.split();
    // Only messages from now on (a hello older than this is stale anyway).
    let since = discovery::now_secs().saturating_sub(10);
    let filter = json!({"kinds": [KIND], "#t": [shared.topic], "since": since});
    tx.send(Message::text(json!(["REQ", "uc", filter]).to_string())).await?;
    // Our hello, if our record is known already (otherwise `announce` sends it).
    if let Some(ev) = shared.event("hello") {
        tx.send(Message::text(ev)).await?;
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut subscribed = false;
    let mut refused = false;
    loop {
        tokio::select! {
            m = rx.next() => {
                let Some(m) = m else { return Ok(()) };
                let m = m?;
                let Ok(text) = m.to_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
                match v[0].as_str() {
                    Some("EVENT") => shared.on_event(&v[2], url),
                    Some("EOSE") if !subscribed => {
                        subscribed = true;
                        *worked = true;
                        tracing::info!("Nostr : relais {url} prêt en {} ms", t.elapsed().as_millis());
                        let _ = shared.events.send(RvEvent::Connected(url.to_string()));
                    }
                    // A refused event (rate limit, policy...): said once.
                    Some("OK") if v[2] == false && !refused => {
                        refused = true;
                        tracing::info!("Nostr : relais {url} refuse nos messages : {}", v[3]);
                    }
                    Some("CLOSED") => anyhow::bail!("abonnement refusé : {}", v[2]),
                    _ => {}
                }
            }
            ev = out.recv() => match ev {
                Ok(ev) => tx.send(Message::text(ev)).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = ping.tick() => tx.send(Message::Ping(Vec::new().into())).await?,
        }
    }
}

fn new_key() -> SigningKey {
    loop {
        if let Ok(k) = SigningKey::from_bytes(&rand::random::<[u8; 32]>()) {
            return k;
        }
    }
}

/// NIP-01 event: id = SHA-256 of the canonical serialization, BIP-340 signature.
fn signed_event(key: &SigningKey, topic: &str, content: &str) -> Value {
    let pubkey = hex::encode(key.verifying_key().to_bytes());
    let created_at = discovery::now_secs();
    let tags = json!([["t", topic]]);
    let ser = json!([0, pubkey, created_at, KIND, tags, content]).to_string();
    let id: [u8; 32] = Sha256::digest(ser.as_bytes()).into();
    let sig = key.sign_raw(&id, &rand::random::<[u8; 32]>()).expect("signature schnorr");
    json!({
        "id": hex::encode(id), "pubkey": pubkey, "created_at": created_at, "kind": KIND,
        "tags": tags, "content": content, "sig": hex::encode(sig.to_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_is_valid_nip01() {
        let key = new_key();
        let ev = signed_event(&key, "abcd", "hello");
        let ser = json!([0, ev["pubkey"], ev["created_at"], ev["kind"], ev["tags"], ev["content"]]).to_string();
        let id: [u8; 32] = Sha256::digest(ser.as_bytes()).into();
        assert_eq!(ev["id"], hex::encode(id));
        let sig = k256::schnorr::Signature::try_from(hex::decode(ev["sig"].as_str().unwrap()).unwrap().as_slice()).unwrap();
        // verify_raw: BIP-340 over the 32-byte id itself.
        key.verifying_key().verify_raw(&id, &sig).unwrap();
    }

    #[test]
    fn only_salon_members_are_heard() {
        let room = Arc::new(Room::new("salon", &[7; 32]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rv = Shared {
            topic: room.nostr_topic(),
            room: room.clone(),
            key: new_key(),
            my_id: "me".into(),
            record: Mutex::new(None),
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
            answered: Mutex::new(HashMap::new()),
            out: broadcast::channel(4).0,
            events: tx,
        };
        let rec = PeerRecord {
            id: "other".into(),
            name: "B".into(),
            avatar: String::new(),
            vip: std::net::Ipv4Addr::new(10, 1, 0, 2),
            ts: discovery::now_secs(),
            punch: None,
            punch_lan: None,
            direct: vec![],
            chat_port: 0,
        };
        let msg = serde_json::to_vec(&Msg { t: "answer".into(), rec }).unwrap();
        // Another salon's key: ignored.
        let stranger = Room::new("salon", &[8; 32]);
        rv.on_event(&signed_event(&new_key(), &rv.topic, &hex::encode(stranger.encrypt(&msg))), "r");
        assert!(rx.try_recv().is_err());
        // A member: heard once, even if every relay forwards it.
        let ev = signed_event(&new_key(), &rv.topic, &hex::encode(room.encrypt(&msg)));
        rv.on_event(&ev, "r1");
        rv.on_event(&ev, "r2");
        assert!(matches!(rx.try_recv(), Ok(RvEvent::Record(r, _)) if r.id == "other"));
        assert!(rx.try_recv().is_err());
    }
}
