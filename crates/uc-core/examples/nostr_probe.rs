//! Prototype: are public Nostr relays a fast rendezvous from here?
//! For each relay, two clients A and B: connection time, whether ephemeral
//! events are accepted, then A <-> B delivery delay over a few round trips.
//! Usage: cargo run -p uc-core --example nostr_probe

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use k256::schnorr::SigningKey;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://relay.nostr.band",
    "wss://nostr.mom",
    "wss://relay.snort.social",
    "wss://offchain.pub",
    "wss://nostr.wine",
];
/// Ephemeral range (20000-29999): relays forward without storing.
const KIND: u32 = 21_337;
const ROUNDS: usize = 5;
const WAIT: Duration = Duration::from_secs(8);

fn new_key() -> SigningKey {
    loop {
        if let Ok(k) = SigningKey::from_bytes(&rand::random::<[u8; 32]>()) {
            return k;
        }
    }
}

/// NIP-01 event, signed (BIP-340 Schnorr over the event id).
fn event(key: &SigningKey, topic: &str, content: &str) -> Value {
    let pubkey = hex::encode(key.verifying_key().to_bytes());
    let created_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let tags = json!([["t", topic]]);
    let ser = json!([0, pubkey, created_at, KIND, tags, content]).to_string();
    let id: [u8; 32] = Sha256::digest(ser.as_bytes()).into();
    let sig = key.sign_raw(&id, &rand::random::<[u8; 32]>()).expect("schnorr");
    json!({
        "id": hex::encode(id), "pubkey": pubkey, "created_at": created_at, "kind": KIND,
        "tags": tags, "content": content, "sig": hex::encode(sig.to_bytes()),
    })
}

struct Client {
    tx: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    rx: mpsc::UnboundedReceiver<Value>,
}

impl Client {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url)).await.context("délai dépassé")??;
        let (tx, mut read) = ws.split();
        let (in_tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(Ok(m)) = read.next().await {
                if let Ok(t) = m.to_text() {
                    if let Ok(v) = serde_json::from_str::<Value>(t) {
                        let _ = in_tx.send(v);
                    }
                }
            }
        });
        Ok(Self { tx, rx })
    }

    async fn send(&mut self, v: Value) -> Result<()> {
        self.tx.send(Message::text(v.to_string())).await?;
        Ok(())
    }

    /// Next message matching `f` (others are dropped), within WAIT.
    async fn wait(&mut self, f: impl Fn(&Value) -> bool) -> Result<Value> {
        let end = Instant::now() + WAIT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(v)) if f(&v) => return Ok(v),
                Ok(Some(_)) => continue,
                Ok(None) => bail!("connexion fermée"),
                Err(_) => bail!("rien reçu en {} s", WAIT.as_secs()),
            }
        }
    }
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

/// Time for an event published by `from` to reach `to` (subscription `sub`).
async fn deliver(from: &mut Client, to: &mut Client, key: &SigningKey, sub: &str, topic: &str, i: usize) -> Result<Duration> {
    let ev = event(key, topic, &format!("ping {i}"));
    let id = ev["id"].as_str().unwrap().to_string();
    let t = Instant::now();
    from.send(json!(["EVENT", ev])).await?;
    match to.wait(|v| v[0] == "EVENT" && v[1] == sub && v[2]["id"] == id.as_str()).await {
        Ok(_) => Ok(t.elapsed()),
        Err(e) => {
            // Why: the relay's OK message usually says (rate limit, pow...).
            let ok = from.wait(|v| v[0] == "OK" && v[1] == id.as_str()).await.ok();
            bail!("message {i} non transmis ({e}){}", ok.map(|o| format!(" · relais : {}", o[3])).unwrap_or_default())
        }
    }
}

/// One relay: returns a report line.
async fn probe(url: &'static str) -> String {
    match probe_inner(url).await {
        Ok(s) => format!("{url:<26} {s}"),
        Err(e) => format!("{url:<26} ÉCHEC : {e:#}"),
    }
}

async fn probe_inner(url: &str) -> Result<String> {
    let t = Instant::now();
    let (a, b) = tokio::join!(Client::connect(url), Client::connect(url));
    let (mut a, mut b) = (a?, b?);
    let connect = t.elapsed();

    let topic = hex::encode(rand::random::<[u8; 8]>());
    let filter = json!({"kinds": [KIND], "#t": [topic]});
    let t = Instant::now();
    a.send(json!(["REQ", "a", filter])).await?;
    b.send(json!(["REQ", "b", filter])).await?;
    a.wait(|v| v[0] == "EOSE").await.context("abonnement")?;
    let eose = t.elapsed();

    let (ka, kb) = (new_key(), new_key());
    let mut one_way = vec![];
    for i in 0..ROUNDS {
        // B -> A ("je suis là"), then A -> B (la réponse).
        one_way.push(deliver(&mut b, &mut a, &kb, "a", &topic, i).await?);
        one_way.push(deliver(&mut a, &mut b, &ka, "b", &topic, i).await?);
    }
    one_way.sort();
    let med = one_way[one_way.len() / 2];
    let max = *one_way.last().unwrap();
    Ok(format!(
        "connexion {:>7} · abonnement {:>7} · transmission médiane {:>7} (max {})",
        ms(connect),
        ms(eose),
        ms(med),
        ms(max)
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Test de {} relais Nostr ({} allers-retours chacun)…\n", RELAYS.len(), ROUNDS);
    let lines = futures::future::join_all(RELAYS.iter().map(|r| probe(r))).await;
    for l in lines {
        println!("{l}");
    }
    Ok(())
}
