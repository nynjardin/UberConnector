//! Times the Nostr fast path of salons (see `uc_core::push`) on the real DHT
//! and relays, with two separate directories as two apps would have: a
//! newcomer knocks, the owner sees it and admits it, then renames the salon.
//! Usage: cargo run -p uc-core --example push_flow -- <scratch dir>

use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;
use uc_core::identity::Identity;
use uc_core::push::{Pushed, What};
use uc_core::salons::{self, Directory, Refresh};

/// Waits for a pushed item of this kind (30 s at most).
async fn wait(rx: &mut Receiver<Pushed>, want: impl Fn(&What) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Ok(Ok(p)) = tokio::time::timeout_at(deadline.into(), rx.recv()).await {
        if want(&p.what) {
            return true;
        }
    }
    false
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let base = std::path::PathBuf::from(args.get(1).expect("dossier de travail"));
    let owner = Identity::load_or_create(&base.join("owner"))?;
    let carl = Identity::load_or_create(&base.join("carl"))?;
    let (a, b) = tokio::try_join!(Directory::start(), Directory::start())?;
    let mut rx_a = a.enable_push().subscribe();
    let mut rx_b = b.enable_push().subscribe();
    let t0 = Instant::now();
    let log = |m: &str| println!("[+{:>5.1}s] {m}", t0.elapsed().as_secs_f32());

    let mut salon = salons::create(&owner, "Salon rapide");
    salon.gated = true;
    a.rotate(&owner, &mut salon).await?;
    a.watch(std::slice::from_ref(&salon));
    log(&format!("salon avec validation publié, code {}", salon.invite));

    let mut c = b.resolve(&carl, &salon.invite).await?;
    b.watch(std::slice::from_ref(&c));
    log(&format!("Carl a lu l'invitation (en attente = {})", c.pending));
    // Lets both subscriptions reach the relays.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let t = Instant::now();
    let (r, seen) = tokio::join!(b.knock(&carl, &c, "Carl", ""), async {
        let ok = wait(&mut rx_a, |w| *w == What::Knock).await;
        (ok, t.elapsed().as_secs_f32())
    });
    r?;
    log(&format!("demande de Carl vue par le propriétaire : {} en {:.1} s (lecture des places libres comprise)", seen.0, seen.1));
    let t = Instant::now();
    let knocks = a.knocks(&salon).await;
    log(&format!("salle d'attente lue en {:.1} s : {:?}", t.elapsed().as_secs_f32(), knocks.iter().map(|k| &k.name).collect::<Vec<_>>()));

    let k = knocks.iter().find(|k| k.id == carl.id).expect("demande de Carl");
    let t = Instant::now();
    let (r, t_push) = tokio::join!(a.admit(&owner, &mut salon, k), async {
        let ok = wait(&mut rx_b, |w| matches!(w, What::Admission { member } if *member == carl.id)).await;
        (ok, t.elapsed().as_secs_f32())
    });
    r?;
    let (pushed, t_push) = t_push;
    let t = Instant::now();
    let ok = b.check_admission(&carl, &mut c).await?;
    log(&format!(
        "admission poussée : {pushed} en {t_push:.1} s ; Carl admis = {ok} (bonne clé = {}) {:.1} s plus tard",
        c.access_key == salon.access_key,
        t.elapsed().as_secs_f32()
    ));

    let t = Instant::now();
    salon.name = "Salon renommé".into();
    let (r, (pushed, t_push)) = tokio::join!(a.publish(&owner, &salon), async {
        let ok = wait(&mut rx_b, |w| *w == What::Changed).await;
        (ok, t.elapsed().as_secs_f32())
    });
    r?;
    let t = Instant::now();
    let r = b.refresh(&carl, &mut c).await?;
    log(&format!(
        "renommage poussé : {pushed} en {t_push:.1} s ; Carl voit « {} » ({}) {:.1} s plus tard",
        c.name,
        if matches!(r, Refresh::Updated) { "mis à jour" } else { "inchangé" },
        t.elapsed().as_secs_f32()
    ));

    salon.deleted = true;
    a.publish(&owner, &salon).await?;
    log("salon de test supprimé");
    Ok(())
}
