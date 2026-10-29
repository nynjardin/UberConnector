//! Hole punching test between two PCs, without UberConnector: each one shows
//! its public address, the user types the other's, then both send UDP
//! packets to each other for 2 minutes. Tells whether the networks let a
//! direct UDP path open at all.
//! Usage: cargo run -p uc-core --example punch_test

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uc_core::punch::{self, Puncher};

const RUN: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = Arc::new(Puncher::bind().await?);
    let me = punch::stun(&sock, &["stun.l.google.com:19302", "stun.cloudflare.com:3478"]).await?;
    println!("Votre adresse publique : {me}");
    println!("Envoyez-la à l'autre PC TELLE QUELLE (port compris), et lancez ce test des deux côtés en même temps.\n");

    // Keeps the NAT mapping alive while the user types.
    let keep = {
        let sock = sock.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let _ = punch::stun(&sock, &["stun.l.google.com:19302"]).await;
            }
        })
    };
    let peer: SocketAddr = loop {
        print!("Adresse affichée par l'autre PC, port compris : ");
        std::io::stdout().flush()?;
        let line = tokio::task::spawn_blocking(|| {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).map(|_| s)
        })
        .await??;
        match line.trim().parse::<SocketAddr>() {
            Ok(a) if a == me => println!("c'est votre propre adresse : il faut celle de l'autre PC"),
            Ok(a) => break a,
            Err(_) => println!("adresse invalide (attendu : IP:port, par exemple 202.22.228.245:61905)"),
        }
    };
    keep.abort();

    println!("\nEnvoi de paquets à {peer} pendant {} s…", RUN.as_secs());
    let start = Instant::now();
    let msg = format!("UCTEST de {me}");
    let sender = {
        let sock = sock.clone();
        let msg = msg.clone();
        tokio::spawn(async move {
            loop {
                let _ = sock.send_to(msg.as_bytes(), peer).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
    };
    let (mut got, mut other) = (0u32, 0u32);
    let mut buf = [0u8; 1500];
    while start.elapsed() < RUN {
        let left = RUN.saturating_sub(start.elapsed());
        // Windows reports ICMP "port unreachable" as a receive error: ignore.
        let Ok(Ok((n, from))) = tokio::time::timeout(left, sock.recv_from(&mut buf)).await else { continue };
        let text = String::from_utf8_lossy(&buf[..n]);
        if !text.starts_with("UCTEST") {
            continue;
        }
        if from == peer {
            got += 1;
            if got == 1 {
                println!("  {:>5.1} s  PREMIER PAQUET REÇU de {from} : « {text} »", start.elapsed().as_secs_f32());
            }
        } else {
            other += 1;
            if other == 1 {
                println!("  {:>5.1} s  paquet reçu d'une AUTRE adresse : {from} (« {text} »)", start.elapsed().as_secs_f32());
            }
        }
    }
    sender.abort();

    println!();
    if got > 0 {
        println!("RÉUSSI : {got} paquets reçus de l'autre PC. Le chemin direct s'ouvre entre vos deux réseaux.");
    } else if other > 0 {
        println!("PARTIEL : l'autre PC arrive, mais depuis une autre adresse que celle saisie (son NAT change de port).");
    } else {
        println!("ÉCHEC : aucun paquet reçu. Vérifiez que l'autre PC a lancé le test en même temps avec votre adresse ;");
        println!("si oui, un de vos deux réseaux (box, opérateur, pare-feu) bloque le perçage entre vous.");
    }
    print!("\nEntrée pour fermer…");
    std::io::stdout().flush()?;
    let _ = std::io::stdin().read_line(&mut String::new());
    Ok(())
}
