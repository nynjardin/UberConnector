//! Automatic port opening check: opens a UDP port on the router, shows the
//! public address, then removes the opening right away.
//! Usage: cargo run -p uc-core --example portmap_check   (UC_UPNP_ONLY=1: UPnP only)

use uc_core::portmap::PortMap;
use uc_core::punch::{self, Puncher};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let lan_ip = punch::lan_ipv4();
    // --list: openings currently on the router (UPnP table, shared with NAT-PMP).
    if std::env::args().any(|a| a == "--list") {
        let mut opts = igd_next::SearchOptions::default();
        opts.bind_addr = (lan_ip, 0).into();
        opts.broadcast_address = (std::net::Ipv4Addr::new(192, 168, 10, 1), 1900).into();
        let gw = igd_next::aio::tokio::search_gateway(opts).await?;
        for i in 0.. {
            let Ok(e) = gw.get_generic_port_mapping_entry(i).await else { break };
            println!("{:?} {} -> {}:{} « {} »", e.protocol, e.external_port, e.internal_client, e.internal_port, e.port_mapping_description);
        }
        return Ok(());
    }
    let sock = Puncher::bind().await?;
    let public = punch::stun(&sock, &["stun.l.google.com:19302"]).await.ok();
    let port = std::net::UdpSocket::bind((lan_ip, 0))?.local_addr()?.port();
    println!("PC : {lan_ip}, port de test {port}, IP publique (STUN) : {public:?}");
    let ip = public.and_then(|a| match a.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        _ => None,
    });
    let t = std::time::Instant::now();
    match PortMap::open(lan_ip, port, ip).await {
        Ok(pm) => {
            println!("OUVERT en {} ms par {} : joignable à {}", t.elapsed().as_millis(), pm.method, pm.external);
            pm.close().await;
            println!("refermé");
        }
        Err(e) => println!("ÉCHEC en {} ms : {e:#}", t.elapsed().as_millis()),
    }
    Ok(())
}
