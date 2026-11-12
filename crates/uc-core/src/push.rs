//! Instant delivery of the salons' signed DHT items through Nostr relays.
//!
//! The DHT stays the reference, but it is slow to read and lags behind a
//! write: a newcomer waited tens of seconds for its admission, a moderator
//! for a knock, members for a rename or a new key. Here, whoever writes such
//! an item on purpose (edit, moderation, admission, knock) also sends it to
//! the salon's Nostr topic. Receivers check the item's own BEP44 signature,
//! keep it next to what the DHT returns (the most recent wins) and react
//! right away. A relay or a member can only replay or drop items, not forge
//! one. Keepalive republishing is never pushed: two moderators would echo
//! each other forever.
//!
//! The topic and the envelope use the salon's base key, as the manifest
//! does: anyone ever invited can read it, relays see nothing.

use crate::nostr::{self, CONNECT_TIMEOUT, KIND, PING_EVERY, RECONNECT, RELAYS};
use crate::room::Room;
use anyhow::{Context, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures::{SinkExt, StreamExt};
use k256::schnorr::SigningKey;
use mainline::MutableItem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::Message;

/// Items kept in memory (a few per salon: manifest, moderators, admissions).
const CACHE_MAX: usize = 512;

/// What a pushed item means for its salon.
#[derive(Clone, Debug, PartialEq)]
pub enum What {
    /// Manifest, moderator item or rotated key: refetch the salon.
    Changed,
    /// Someone knocked in the waiting room.
    Knock,
    /// An owner or moderator admitted `member`.
    Admission { member: String },
}

#[derive(Clone, Debug)]
pub struct Pushed {
    pub salon_id: String,
    pub what: What,
}

/// A salon we listen to.
pub struct Watched {
    pub salon_id: String,
    /// Never-rotated base key.
    pub base_key: [u8; 32],
    /// Public key of the salon's waiting room (knock slots).
    pub lobby_key: [u8; 32],
}

struct Entry {
    salon_id: String,
    room: Room,
    lobby_key: [u8; 32],
}

/// A BEP44 item on the wire.
#[derive(Serialize, Deserialize)]
struct Wire {
    k: String,
    s: String,
    q: i64,
    g: String,
    v: String,
}

pub struct Push {
    key: SigningKey,
    my_pubkey: String,
    salons: Mutex<HashMap<String, Entry>>,
    topics: watch::Sender<Vec<String>>,
    out: broadcast::Sender<String>,
    cache: Mutex<HashMap<(Vec<u8>, Vec<u8>), MutableItem>>,
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
    events: broadcast::Sender<Pushed>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl Drop for Push {
    fn drop(&mut self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

fn push_room(salon_id: &str, base_key: &[u8; 32]) -> Room {
    Room::new(&format!("{salon_id}/push"), base_key)
}

/// What BEP44 signs (same encoding as the DHT).
fn signable(seq: i64, value: &[u8], salt: &[u8]) -> Vec<u8> {
    let mut b = vec![];
    if !salt.is_empty() {
        b.extend(format!("4:salt{}:", salt.len()).into_bytes());
        b.extend(salt);
    }
    b.extend(format!("3:seqi{seq}e1:v{}:", value.len()).into_bytes());
    b.extend(value);
    b
}

/// Rebuilds an item from the wire, only if its signature holds.
fn verified(w: &Wire) -> Option<MutableItem> {
    let key: [u8; 32] = hex::decode(&w.k).ok()?.try_into().ok()?;
    let sig: [u8; 64] = hex::decode(&w.g).ok()?.try_into().ok()?;
    let salt = hex::decode(&w.s).ok()?;
    let value = hex::decode(&w.v).ok()?;
    VerifyingKey::from_bytes(&key)
        .ok()?
        .verify(&signable(w.q, &value, &salt), &Signature::from_bytes(&sig))
        .ok()?;
    let salt = (!salt.is_empty()).then_some(salt.as_slice());
    Some(MutableItem::new_signed_unchecked(key, sig, &value, w.q, salt))
}

/// The meaning of an item for `e`'s salon; `None`: not one of its items.
fn classify(e: &Entry, item: &MutableItem) -> Option<What> {
    let salt = std::str::from_utf8(item.salt()?).ok()?;
    let id = &e.salon_id;
    if let Some(member) = salt.strip_prefix(&format!("uc-admit/{id}/")) {
        return Some(What::Admission { member: member.to_string() });
    }
    if salt.starts_with("uc-knock-") && *item.key() == e.lobby_key {
        return Some(What::Knock);
    }
    if salt == format!("uc-salon/{id}") || salt == format!("uc-salon/{id}/mod") || salt.starts_with(&format!("uc-rekey/{id}/")) {
        return Some(What::Changed);
    }
    None
}

impl Push {
    pub fn start() -> Arc<Self> {
        let key = nostr::new_key();
        let my_pubkey = hex::encode(key.verifying_key().to_bytes());
        let p = Arc::new(Self {
            key,
            my_pubkey,
            salons: Default::default(),
            topics: watch::channel(vec![]).0,
            out: broadcast::channel(64).0,
            cache: Default::default(),
            seen: Default::default(),
            events: broadcast::channel(64).0,
            tasks: Default::default(),
        });
        // The relay tasks hold a weak reference: dropping `Push` ends them.
        let tasks = RELAYS.iter().map(|url| tokio::spawn(relay_loop(Arc::downgrade(&p), url)).abort_handle()).collect();
        *p.tasks.lock().unwrap() = tasks;
        p
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Pushed> {
        self.events.subscribe()
    }

    /// The salons to listen to (all of them, replaces the previous list).
    pub fn watch(&self, salons: Vec<Watched>) {
        let mut map = HashMap::new();
        for w in salons {
            let room = push_room(&w.salon_id, &w.base_key);
            map.insert(room.nostr_topic(), Entry { salon_id: w.salon_id, room, lobby_key: w.lobby_key });
        }
        let mut topics: Vec<String> = map.keys().cloned().collect();
        topics.sort();
        *self.salons.lock().unwrap() = map;
        self.topics.send_if_modified(|t| {
            let changed = *t != topics;
            *t = topics;
            changed
        });
    }

    /// Sends an item we just wrote to the members of its salon.
    pub fn send(&self, salon_id: &str, base_key: &[u8; 32], item: &MutableItem) {
        self.remember(item.clone());
        let wire = Wire {
            k: hex::encode(item.key()),
            s: hex::encode(item.salt().unwrap_or_default()),
            q: item.seq(),
            g: hex::encode(item.signature()),
            v: hex::encode(item.value()),
        };
        let Ok(plain) = serde_json::to_vec(&wire) else { return };
        let room = push_room(salon_id, base_key);
        let ev = nostr::signed_event(&self.key, &room.nostr_topic(), &hex::encode(room.encrypt(&plain)));
        let _ = self.out.send(json!(["EVENT", ev]).to_string());
    }

    /// The most recent pushed copy of an item, if any.
    pub fn cached(&self, key: &[u8; 32], salt: Option<&[u8]>) -> Option<MutableItem> {
        self.cache.lock().unwrap().get(&(key.to_vec(), salt.unwrap_or_default().to_vec())).cloned()
    }

    /// Keeps `item` if it is newer than our copy; returns whether it is.
    fn remember(&self, item: MutableItem) -> bool {
        let k = (item.key().to_vec(), item.salt().unwrap_or_default().to_vec());
        let mut cache = self.cache.lock().unwrap();
        if cache.get(&k).is_some_and(|old| old.seq() >= item.seq()) {
            return false;
        }
        if cache.len() >= CACHE_MAX && !cache.contains_key(&k) {
            cache.clear();
        }
        cache.insert(k, item);
        true
    }

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

    fn on_event(&self, ev: &Value) {
        let (Some(id), Some(topic)) = (ev["id"].as_str(), ev["tags"][0][1].as_str()) else { return };
        if ev["pubkey"].as_str() == Some(self.my_pubkey.as_str()) || !self.first_sight(id) {
            return;
        }
        let pushed = {
            let salons = self.salons.lock().unwrap();
            let Some(e) = salons.get(topic) else { return };
            let Some(item) = ev["content"]
                .as_str()
                .and_then(|c| hex::decode(c).ok())
                .and_then(|b| e.room.decrypt(&b).ok())
                .and_then(|p| serde_json::from_slice::<Wire>(&p).ok())
                .and_then(|w| verified(&w))
            else {
                return;
            };
            let Some(what) = classify(e, &item) else { return };
            // A replayed older copy changes nothing.
            if !self.remember(item) {
                return;
            }
            Pushed { salon_id: e.salon_id.clone(), what }
        };
        tracing::info!("Nostr : {:?} reçu pour le salon {}", pushed.what, pushed.salon_id);
        let _ = self.events.send(pushed);
    }
}

/// One relay, reconnected whenever it drops, idle while no salon is watched.
async fn relay_loop(push: std::sync::Weak<Push>, url: &'static str) {
    let mut quiet = false;
    loop {
        let Some(p) = push.upgrade() else { return };
        let mut topics = p.topics.subscribe();
        drop(p);
        if topics.wait_for(|t| !t.is_empty()).await.is_err() {
            return;
        }
        let mut worked = false;
        let r = relay_session(&push, url, &mut worked).await;
        if worked {
            quiet = false;
        }
        if !quiet {
            let why = r.err().map_or("connexion fermée".into(), |e| format!("{e:#}"));
            tracing::info!("Nostr (salons) : relais {url} indisponible ({why}), nouvel essai toutes les {} s", RECONNECT.as_secs());
            quiet = true;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

async fn relay_session(push: &std::sync::Weak<Push>, url: &str, worked: &mut bool) -> Result<()> {
    let (mut out, mut topics) = {
        let p = push.upgrade().context("arrêt")?;
        (p.out.subscribe(), p.topics.subscribe())
    };
    let t = Instant::now();
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .context("délai dépassé")??;
    let (mut tx, mut rx) = ws.split();
    // Same subscription id: a new list replaces the previous one.
    let req = |topics: &[String]| {
        let since = crate::discovery::now_secs().saturating_sub(10);
        json!(["REQ", "ucp", {"kinds": [KIND], "#t": topics, "since": since}]).to_string()
    };
    let list = topics.borrow_and_update().clone();
    tx.send(Message::text(req(&list))).await?;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut subscribed = false;
    loop {
        tokio::select! {
            m = rx.next() => {
                let Some(m) = m else { return Ok(()) };
                let m = m?;
                let Ok(text) = m.to_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
                match v[0].as_str() {
                    Some("EVENT") => {
                        let Some(p) = push.upgrade() else { return Ok(()) };
                        p.on_event(&v[2]);
                    }
                    Some("EOSE") if !subscribed => {
                        subscribed = true;
                        *worked = true;
                        tracing::info!("Nostr (salons) : relais {url} prêt en {} ms", t.elapsed().as_millis());
                    }
                    Some("CLOSED") => anyhow::bail!("abonnement refusé : {}", v[2]),
                    _ => {}
                }
            }
            r = topics.changed() => {
                if r.is_err() {
                    return Ok(());
                }
                let list = topics.borrow_and_update().clone();
                if list.is_empty() {
                    tx.send(Message::text(json!(["CLOSE", "ucp"]).to_string())).await?;
                    return Ok(());
                }
                tx.send(Message::text(req(&list))).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(push: &Push, id: &str, base: [u8; 32]) {
        push.watch(vec![Watched { salon_id: id.into(), base_key: base, lobby_key: [9; 32] }]);
    }

    /// The event another member would send for `item`.
    fn event_for(id: &str, base: &[u8; 32], item: &MutableItem) -> Value {
        let wire = Wire {
            k: hex::encode(item.key()),
            s: hex::encode(item.salt().unwrap_or_default()),
            q: item.seq(),
            g: hex::encode(item.signature()),
            v: hex::encode(item.value()),
        };
        let room = push_room(id, base);
        nostr::signed_event(&nostr::new_key(), &room.nostr_topic(), &hex::encode(room.encrypt(&serde_json::to_vec(&wire).unwrap())))
    }

    #[tokio::test]
    async fn signed_items_are_kept_and_classified() {
        let push = Push::start();
        let base = [5u8; 32];
        watched(&push, "s1", base);
        let mut rx = push.subscribe();
        let owner = mainline::SigningKey::from_bytes(&[3; 32]);
        let item = MutableItem::new(owner.clone(), b"manifest", 10, Some(b"uc-salon/s1"));
        push.on_event(&event_for("s1", &base, &item));
        let got = rx.try_recv().unwrap();
        assert_eq!((got.salon_id.as_str(), got.what), ("s1", What::Changed));
        assert_eq!(push.cached(item.key(), Some(b"uc-salon/s1")).unwrap().value(), b"manifest");

        // An older copy (replay) is ignored.
        let old = MutableItem::new(owner.clone(), b"old", 5, Some(b"uc-salon/s1"));
        push.on_event(&event_for("s1", &base, &old));
        assert!(rx.try_recv().is_err());
        assert_eq!(push.cached(item.key(), Some(b"uc-salon/s1")).unwrap().value(), b"manifest");

        let admit = MutableItem::new(owner, b"key", 11, Some(b"uc-admit/s1/abcd"));
        push.on_event(&event_for("s1", &base, &admit));
        assert_eq!(rx.try_recv().unwrap().what, What::Admission { member: "abcd".into() });
    }

    #[tokio::test]
    async fn forged_or_foreign_items_are_dropped() {
        let push = Push::start();
        let base = [5u8; 32];
        watched(&push, "s1", base);
        let mut rx = push.subscribe();
        let owner = mainline::SigningKey::from_bytes(&[3; 32]);
        let item = MutableItem::new(owner, b"manifest", 10, Some(b"uc-salon/s1"));
        // Value changed after signing.
        let forged = MutableItem::new_signed_unchecked(*item.key(), *item.signature(), b"evil", 10, item.salt());
        push.on_event(&event_for("s1", &base, &forged));
        // Encrypted with another salon's key.
        push.on_event(&event_for("s1", &[6u8; 32], &item));
        assert!(rx.try_recv().is_err());
        assert!(push.cached(item.key(), Some(b"uc-salon/s1")).is_none());
    }
}
