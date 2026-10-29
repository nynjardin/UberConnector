//! NAT behaviour check: the same UDP socket asks two STUN servers for its
//! public address. Different public ports = "symmetric" NAT (hole punching
//! fails between two such NATs: a relay is needed).
//! Usage: cargo run -p uc-core --example nat_check

use uc_core::punch::{self, Puncher};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = Puncher::bind().await?;
    println!("socket local : port {}", sock.local_addr()?.port());
    let mut seen = vec![];
    for server in ["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun.l.google.com:19305"] {
        match punch::stun(&sock, &[server]).await {
            Ok(a) => {
                println!("{server:<28} -> {a}");
                seen.push(a);
            }
            Err(e) => println!("{server:<28} -> échec : {e:#}"),
        }
    }
    seen.dedup();
    println!(
        "{}",
        match seen.len() {
            0 => "aucune réponse STUN".to_string(),
            1 => "NAT « cône » : le même port public pour toutes les destinations, le perçage est possible".to_string(),
            _ => "NAT SYMÉTRIQUE : le port public change selon la destination, le perçage échouera avec un autre NAT symétrique".to_string(),
        }
    );
    Ok(())
}
