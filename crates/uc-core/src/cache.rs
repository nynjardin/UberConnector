//! Remembered between sessions, to connect without waiting for the DHT:
//! - DHT nodes (`dht-nodes.json`): bootstrap from nodes we already know;
//! - per salon (`salon-cache/<id>.json`): our EasyTier port, kept stable so
//!   members find us at the same address, and the members' last records,
//!   tried as soon as a session starts.

use crate::discovery::PeerRecord;
use anyhow::Result;
use mainline::Dht;
use mainline::async_dht::AsyncDht;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Members not seen for this long are forgotten.
const MEMBER_TTL_SECS: u64 = 30 * 24 * 3600;
const MAX_MEMBERS: usize = 32;
const MAX_NODES: usize = 200;

/// A DHT client bootstrapped from the default nodes plus those we knew.
pub fn dht(data_dir: Option<&Path>) -> Result<AsyncDht> {
    let known: Vec<String> = data_dir
        .and_then(|d| std::fs::read_to_string(d.join("dht-nodes.json")).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // `extra_bootstrap` alone would *replace* the default nodes.
    let nodes: Vec<String> = mainline::DEFAULT_BOOTSTRAP_NODES.iter().map(|s| s.to_string()).chain(known).collect();
    Ok(Dht::builder().bootstrap(&nodes).build()?.as_async())
}

/// Saves the routing table for the next start.
pub async fn save_dht_nodes(dht: &AsyncDht, data_dir: &Path) {
    let mut nodes = dht.to_bootstrap().await;
    nodes.truncate(MAX_NODES);
    if nodes.len() >= 8 {
        if let Ok(text) = serde_json::to_string(&nodes) {
            let _ = std::fs::write(data_dir.join("dht-nodes.json"), text);
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct SalonCache {
    #[serde(default)]
    pub et_port: u16,
    #[serde(default)]
    pub members: Vec<PeerRecord>,
}

fn path(data_dir: &Path, salon_id: &str) -> PathBuf {
    data_dir.join("salon-cache").join(format!("{salon_id}.json"))
}

impl SalonCache {
    pub fn load(data_dir: &Path, salon_id: &str) -> Self {
        std::fs::read_to_string(path(data_dir, salon_id))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path, salon_id: &str) {
        let p = path(data_dir, salon_id);
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, text);
        }
    }

    /// Members worth trying (seen recently enough), except us.
    pub fn members(&self, my_id: &str) -> Vec<PeerRecord> {
        let now = crate::discovery::now_secs();
        self.members
            .iter()
            .filter(|r| r.id != my_id && now.saturating_sub(r.ts) < MEMBER_TTL_SECS)
            .cloned()
            .collect()
    }

    /// Keeps the newest record of each member.
    pub fn remember(&mut self, recs: &[PeerRecord]) {
        for r in recs {
            match self.members.iter_mut().find(|m| m.id == r.id) {
                Some(m) if m.ts >= r.ts => {}
                Some(m) => *m = r.clone(),
                None => self.members.push(r.clone()),
            }
        }
        self.members.sort_by(|a, b| b.ts.cmp(&a.ts));
        self.members.truncate(MAX_MEMBERS);
    }
}

/// `wanted` if it is still free (UDP and TCP), otherwise any free port.
pub fn port_or_free(wanted: u16) -> Result<u16> {
    if wanted != 0
        && std::net::UdpSocket::bind(("0.0.0.0", wanted)).is_ok()
        && std::net::TcpListener::bind(("0.0.0.0", wanted)).is_ok()
    {
        return Ok(wanted);
    }
    crate::easytier::free_port()
}
