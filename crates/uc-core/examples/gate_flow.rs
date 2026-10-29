//! End-to-end check of gated salons on the real DHT: gating, waiting room,
//! admission by a moderator, then opening the salon again.
//! Usage: cargo run -p uc-core --example gate_flow -- <scratch dir>
//! With `--knock <salon code> <name>`: knocks as a newcomer and waits for the
//! admission (to test the app's waiting room list). With `--host`: creates a
//! gated salon and admits every newcomer for 5 minutes (to test the app as
//! newcomer).

use uc_core::identity::Identity;
use uc_core::salons::{self, Directory, Member};

fn member(i: &Identity, name: &str) -> Member {
    Member { id: i.id.clone(), pk: i.public_key_hex(), xpk: i.x25519_public_hex(), name: name.into() }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let base = std::path::PathBuf::from(args.get(1).expect("dossier de travail"));
    let dir = Directory::start().await?;
    let t0 = std::time::Instant::now();
    let log = |m: &str| println!("[+{:>3}s] {m}", t0.elapsed().as_secs());

    if args.get(2).map(String::as_str) == Some("--knock") {
        let me = Identity::load_or_create(&base.join("newcomer"))?;
        let mut s = dir.resolve(&me, &args[3]).await?;
        log(&format!("invitation lue : en attente = {}", s.pending));
        for round in 0.. {
            if round % 3 == 0 {
                dir.knock(&me, &s, &args[4], "🐣|#f2b84b").await?;
                log("demande publiée");
            }
            if dir.check_admission(&me, &mut s).await? {
                log(&format!("ACCEPTÉ : clé reçue, époque {}", s.epoch));
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        }
    }

    if args.get(2).map(String::as_str) == Some("--host") {
        let host = Identity::load_or_create(&base.join("host"))?;
        let mut salon = salons::create(&host, "Salon de l'hôte");
        salon.gated = true;
        dir.rotate(&host, &mut salon).await?;
        log(&format!("salon avec validation, code {}", salon.invite));
        for _ in 0..20 {
            for k in dir.knocks(&salon).await {
                if !salon.roster.iter().any(|m| m.id == k.id) {
                    dir.admit(&host, &mut salon, &k).await?;
                    log(&format!("{} accepté", k.name));
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
        salon.deleted = true;
        dir.publish(&host, &salon).await?;
        log("salon de test supprimé");
        return Ok(());
    }

    let owner = Identity::load_or_create(&base.join("owner"))?;
    let modo = Identity::load_or_create(&base.join("modo"))?;
    let carl = Identity::load_or_create(&base.join("carl"))?;
    let dave = Identity::load_or_create(&base.join("dave"))?;

    let mut salon = salons::create(&owner, "Salon privé");
    salon.remember(member(&modo, "Modo"));
    salon.mods.push(modo.public_key_hex());
    dir.publish(&owner, &salon).await?;
    let mut m = dir.resolve(&modo, &salon.invite).await?;
    log(&format!("créé ; le modérateur a la clé : {}", m.access_key == salon.access_key));

    // Gating changes the key (the old invite gave it).
    salon.gated = true;
    let sent = dir.rotate(&owner, &mut salon).await?;
    dir.refresh(&modo, &mut m).await?;
    log(&format!("validation activée, clé remise à {sent} ; le modérateur suit : {}", m.access_key == salon.access_key));

    let mut c = dir.resolve(&carl, &salon.invite).await?;
    log(&format!(
        "Carl avec le code : en attente = {}, a la vraie clé = {}",
        c.pending,
        c.access_key == salon.access_key
    ));
    dir.knock(&carl, &c, "Carl", "🐣|#f2b84b").await?;
    log("Carl frappe à la porte");

    let mut knocks = vec![];
    for _ in 0..5 {
        knocks = dir.knocks(&m).await;
        if !knocks.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    log(&format!("le modérateur voit en attente : {:?}", knocks.iter().map(|k| &k.name).collect::<Vec<_>>()));
    log(&format!("Carl avant validation : admis = {}", dir.check_admission(&carl, &mut c).await?));

    let k = knocks.iter().find(|k| k.id == carl.id).expect("demande de Carl");
    dir.admit(&modo, &mut m, k).await?;
    let ok = dir.check_admission(&carl, &mut c).await?;
    log(&format!(
        "le modérateur accepte ; Carl admis = {ok}, bonne clé = {}, en attente = {}",
        c.access_key == salon.access_key,
        c.pending
    ));

    // Opening the salon: the code gives the key again.
    salon.gated = false;
    dir.publish(&owner, &salon).await?;
    let d = dir.resolve(&dave, &salon.invite).await?;
    log(&format!(
        "salon rouvert ; Dave avec le code : en attente = {}, bonne clé = {}",
        d.pending,
        d.access_key == salon.access_key
    ));

    salon.deleted = true;
    dir.publish(&owner, &salon).await?;
    log("salon de test supprimé");
    Ok(())
}
