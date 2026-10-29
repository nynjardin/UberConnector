//! Non-admin smoke test of DHT discovery + hole punching (no EasyTier).
//! Usage: cargo run -p uc-core --example discovery_probe -- <CODE> <NAME>

use std::sync::Arc;
use std::time::{Duration, Instant};
use uc_core::discovery::{self, Discovery, PeerRecord};
use uc_core::punch::{self, PunchEvent, Puncher};
use uc_core::room::Room;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let room = Arc::new(Room::new(&args[1], &[7; 32]));
    let name = args[2].clone();
    let id = hex::encode(rand::random::<[u8; 8]>());
    let t0 = Instant::now();
    let log = |m: String| println!("[{name} +{:>3}s] {m}", t0.elapsed().as_secs());

    let sock = Puncher::bind().await?;
    let public = punch::stun(&sock, &["stun.l.google.com:19302", "stun.cloudflare.com:3478"]).await.ok();
    log(format!("STUN: {public:?}"));
    let disco = Discovery::start(room.clone()).await?;
    log("DHT bootstrapped".into());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let puncher = Puncher::new(sock, &id, punch::lan_ipv4(), 9, true, tx);
    let rec = PeerRecord {
        id: id.clone(),
        name: name.clone(),
        avatar: String::new(),
        vip: "10.21.0.1".parse()?,
        ts: 0,
        punch: public,
        punch_lan: Some(puncher.lan_addr()),
        direct: vec![],
        chat_port: 0,
    };
    let mut slots = disco.read_slots(|_| {}).await;
    for _ in 0..8 {
        let mut r = rec.clone();
        r.ts = discovery::now_secs();
        let slot = disco.publish(&r, &slots).await?;
        slots = disco.read_slots(|_| {}).await;
        let others = discovery::others(&slots, &id);
        log(format!("slot {slot}, autres: {:?}", others.iter().map(|o| &o.name).collect::<Vec<_>>()));
        for o in &others {
            puncher.upsert_peer(o, &id).await?;
        }
        let deadline = tokio::time::sleep(Duration::from_secs(8));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                Some(e) = rx.recv() => {
                    if let PunchEvent::Established { id, remote, dialer, .. } = e {
                        log(format!("PERCÉ avec {id} via {remote} (dialer={dialer})"));
                    }
                }
            }
        }
    }
    Ok(())
}
