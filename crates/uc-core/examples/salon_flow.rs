//! End-to-end check of the salon lifecycle on the real DHT (no admin):
//! invite, ban + key rotation, moderator, deletion.
//! Usage: cargo run -p uc-core --example salon_flow -- <scratch dir>

use uc_core::identity::Identity;
use uc_core::salons::{self, Directory, Member, Salon};

fn member(i: &Identity, name: &str) -> Member {
    Member { id: i.id.clone(), pk: i.public_key_hex(), xpk: i.x25519_public_hex(), name: name.into() }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::path::PathBuf::from(std::env::args().nth(1).expect("dossier de travail"));
    let owner = Identity::load_or_create(&base.join("owner"))?;
    let alice = Identity::load_or_create(&base.join("alice"))?;
    let bob = Identity::load_or_create(&base.join("bob"))?;
    let dir = Directory::start().await?;
    let t0 = std::time::Instant::now();
    let log = |m: &str| println!("[+{:>3}s] {m}", t0.elapsed().as_secs());

    let mut salon = salons::create(&owner, "Burnout du jeudi");
    dir.publish(&owner, &salon).await?;
    log(&format!("créé, invitation {}", salon.invite));

    let mut a: Salon = dir.resolve(&alice, &salon.invite).await?;
    let mut b: Salon = dir.resolve(&bob, &salon.invite).await?;
    log(&format!("Alice et Bob ont rejoint (même clé : {})", a.access_key == salon.access_key && b.access_key == salon.access_key));

    // The owner met both in the chat.
    salon.remember(member(&alice, "Alice"));
    salon.remember(member(&bob, "Bob"));

    // Ban Bob: owner's list + rotation.
    salon.owner_banned.push(bob.id.clone());
    salon.banned.push(bob.id.clone());
    let old_key = salon.access_key.clone();
    let sent = dir.rotate(&owner, &mut salon).await?;
    log(&format!("Bob banni : nouvelle clé remise à {sent} membre(s), nouveau code {}", salon.invite));

    dir.refresh(&alice, &mut a).await?;
    log(&format!(
        "Alice : nouvelle clé = {} (identique au propriétaire : {}), nouveau code = {}",
        a.access_key != old_key,
        a.access_key == salon.access_key,
        a.invite == salon.invite
    ));
    dir.refresh(&bob, &mut b).await?;
    log(&format!(
        "Bob : se voit banni = {}, a la nouvelle clé = {}, clé perdue = {}",
        b.banned.contains(&bob.id),
        b.access_key == salon.access_key,
        b.key_lost
    ));
    match dir.resolve(&bob, &salon.invite).await {
        Err(e) => log(&format!("Bob avec le nouveau code : {e}")),
        Ok(_) => log("ERREUR : Bob a pu rejoindre"),
    }

    // Alice becomes moderator and regenerates the code.
    salon.mods.push(alice.public_key_hex());
    dir.publish(&owner, &salon).await?;
    dir.refresh(&alice, &mut a).await?;
    log(&format!("Alice modératrice : {}", a.can_moderate(&alice)));
    a.remember(member(&owner, "Owner"));
    let key_before = a.access_key.clone();
    dir.rotate(&alice, &mut a).await?;
    dir.refresh(&owner, &mut salon).await?;
    log(&format!(
        "code régénéré par Alice : le propriétaire a la nouvelle clé = {}",
        salon.access_key == a.access_key && salon.access_key != key_before
    ));

    salon.deleted = true;
    dir.publish(&owner, &salon).await?;
    dir.refresh(&alice, &mut a).await?;
    log(&format!("après suppression : supprimé côté Alice = {}", a.deleted));
    Ok(())
}
