//! Prints what a salon currently holds in the DHT (diagnostic, no admin).
//! Usage: cargo run -p uc-core --example room_dump -- <INVITE CODE> <uc-data dir>

use std::sync::Arc;
use uc_core::discovery::{Discovery, now_secs};
use uc_core::identity::Identity;
use uc_core::salons::Directory;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let code = std::env::args().nth(1).expect("code d'invitation");
    let data = std::env::args().nth(2).expect("dossier uc-data");
    let me = Identity::load_or_create(std::path::Path::new(&data))?;
    let salon = Directory::start().await?.resolve(&me, &code).await?;
    println!("Salon « {} » id={} propriétaire={}…", salon.name, salon.id, &salon.owner[..16]);
    let disco = Discovery::start(Arc::new(salon.room())).await?;
    println!("IP publique vue par la DHT : {:?}", disco.public_ipv4().await);
    for (i, slot) in disco.read_slots(|_| {}).await.iter().enumerate() {
        if let Some(r) = slot {
            println!(
                "slot {i:>2}: {} ({}) vip={} il y a {}s punch={:?} lan={:?}",
                r.name, r.id, r.vip, now_secs().saturating_sub(r.ts), r.punch, r.punch_lan
            );
        }
    }
    Ok(())
}
