//! Serverless peer discovery over the BitTorrent mainline DHT (BEP44).
//!
//! Every member of a room knows the room signing key (derived from the code),
//! so anyone can write mutable items under it. We use `SLOTS` salts; each peer
//! keeps one slot with its encrypted `PeerRecord` fresh, and reads all slots
//! to learn about the others.

use crate::room::Room;
use anyhow::Result;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use mainline::async_dht::AsyncDht;
use mainline::{Dht, MutableItem};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

pub const SLOTS: usize = 16;
/// A record older than this is considered gone and its slot reusable.
pub const STALE_SECS: u64 = 180;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PeerRecord {
    pub id: String,
    pub name: String,
    /// "emoji|#rrggbb" (see the app's profile dialog).
    #[serde(default)]
    pub avatar: String,
    pub vip: Ipv4Addr,
    pub ts: u64,
    /// Public (STUN-mapped) address of the hole punching socket.
    pub punch: Option<SocketAddr>,
    /// LAN address of the hole punching socket (players on the same LAN).
    pub punch_lan: Option<SocketAddr>,
    /// Direct EasyTier URLs (works when a port is forwarded / UPnP).
    pub direct: Vec<String>,
    /// Chat port, reached through the salon network at `vip`.
    #[serde(default)]
    pub chat_port: u16,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn salt(i: usize) -> Vec<u8> {
    format!("uc-slot-{i}").into_bytes()
}

pub struct Discovery {
    dht: AsyncDht,
    room: Arc<Room>,
    my_slot: std::sync::Mutex<Option<usize>>,
}

impl Discovery {
    pub async fn start(room: Arc<Room>) -> Result<Self> {
        let dht = Dht::client()?.as_async();
        if !dht.bootstrapped().await {
            anyhow::bail!("impossible de rejoindre la DHT (pas d'accès Internet ?)");
        }
        Ok(Self { dht, room, my_slot: Default::default() })
    }

    /// Without waiting for the bootstrap (queries work meanwhile), from the
    /// DHT nodes remembered in `data_dir`.
    pub fn new(room: Arc<Room>, data_dir: &Path) -> Result<Self> {
        Ok(Self { dht: crate::cache::dht(Some(data_dir))?, room, my_slot: Default::default() })
    }

    /// False when the DHT cannot be reached at all.
    pub async fn bootstrapped(&self) -> bool {
        self.dht.bootstrapped().await
    }

    pub async fn save_nodes(&self, data_dir: &Path) {
        crate::cache::save_dht_nodes(&self.dht, data_dir).await
    }

    /// Reads every slot. `None` entries are empty, undecryptable or garbage.
    /// `on_record` gets each record as soon as it arrives: one slow (empty)
    /// slot must not delay the members already found.
    pub async fn read_slots(&self, mut on_record: impl FnMut(&PeerRecord)) -> Vec<Option<PeerRecord>> {
        let key = self.room.dht_public_key();
        let mut pending: FuturesUnordered<_> = (0..SLOTS)
            .map(|i| {
                let dht = self.dht.clone();
                let s = salt(i);
                async move { (i, dht.get_mutable_most_recent(&key, Some(&s)).await) }
            })
            .collect();
        let mut slots = vec![None; SLOTS];
        while let Some((i, item)) = pending.next().await {
            let rec: Option<PeerRecord> = item
                .and_then(|item| self.room.decrypt(item.value()).ok())
                .and_then(|plain| serde_json::from_slice(&plain).ok());
            if let Some(r) = &rec {
                on_record(r);
            }
            slots[i] = rec;
        }
        slots
    }

    /// Picks (or keeps) a slot for us and writes our record in it.
    pub async fn publish(&self, rec: &PeerRecord, slots: &[Option<PeerRecord>]) -> Result<usize> {
        let now = now_secs();
        let owned = slots.iter().position(|s| s.as_ref().is_some_and(|r| r.id == rec.id));
        let slot = match owned {
            Some(i) => i,
            None => {
                let free: Vec<usize> = (0..SLOTS)
                    .filter(|&i| match &slots[i] {
                        None => true,
                        Some(r) => now.saturating_sub(r.ts) > STALE_SECS,
                    })
                    .collect();
                // Random pick among free slots limits races between joiners.
                if free.is_empty() {
                    rand::random_range(0..SLOTS)
                } else {
                    free[rand::random_range(0..free.len())]
                }
            }
        };
        let value = self.room.encrypt(&serde_json::to_vec(rec)?);
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        let s = salt(slot);
        let item = MutableItem::new(self.room.signing_key.clone(), &value, seq, Some(&s));
        self.dht.put_mutable(item, None).await?;
        *self.my_slot.lock().unwrap() = Some(slot);
        Ok(slot)
    }

    pub fn my_slot(&self) -> Option<usize> {
        *self.my_slot.lock().unwrap()
    }

    pub async fn public_ipv4(&self) -> Option<Ipv4Addr> {
        self.dht.info().await.public_address().map(|a| *a.ip())
    }
}

/// Fresh record of another member.
pub fn is_other(r: &PeerRecord, my_id: &str) -> bool {
    r.id != my_id && now_secs().saturating_sub(r.ts) <= STALE_SECS
}

/// Fresh records of the other members.
pub fn others(slots: &[Option<PeerRecord>], my_id: &str) -> Vec<PeerRecord> {
    let now = now_secs();
    let mut out: Vec<PeerRecord> = slots
        .iter()
        .flatten()
        .filter(|r| is_other(r, my_id) && r.ts <= now + STALE_SECS)
        .cloned()
        .collect();
    // A member may still have its previous session's record in another slot:
    // keep the newest (the old one leads to a dead address).
    out.sort_by(|a, b| a.id.cmp(&b.id).then(b.ts.cmp(&a.ts)));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, ts: u64, port: u16) -> Option<PeerRecord> {
        Some(PeerRecord {
            id: id.into(),
            name: id.into(),
            avatar: String::new(),
            vip: Ipv4Addr::new(10, 1, 0, 2),
            ts,
            punch: Some(SocketAddr::from(([1, 2, 3, 4], port))),
            punch_lan: None,
            direct: vec![],
            chat_port: 0,
        })
    }

    #[test]
    fn newest_record_wins() {
        let now = now_secs();
        // Previous session (old port) in a lower slot than the current one.
        let slots = vec![rec("bb", now - 60, 1111), None, rec("bb", now - 5, 2222), rec("aa", now, 3333), rec("me", now, 4444)];
        let out = others(&slots, "me");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].punch.unwrap().port(), 2222);
    }
}
