//! Automatic port opening on the home router (NAT-PMP, then UPnP IGD), so
//! that members can join us directly even when hole punching fails between
//! two networks. Only EasyTier's UDP port is opened (EasyTier refuses
//! whoever lacks the salon key); the mapping is renewed while the session
//! runs, removed when it ends, and expires by itself after a crash.

use anyhow::{Context, Result, bail};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

/// Mapping lifetime, renewed at half time.
const LEASE: u32 = 20 * 60;
const NATPMP_PORT: u16 = 5351;
const DESCRIPTION: &str = "UberConnector";

#[derive(Clone)]
enum Method {
    NatPmp { gateway: Ipv4Addr },
    Upnp(igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>),
}

pub struct PortMap {
    /// Public address members can reach us at.
    pub external: SocketAddrV4,
    pub method: &'static str,
    lan: SocketAddrV4,
    how: Method,
    renew: JoinHandle<()>,
}

impl Drop for PortMap {
    fn drop(&mut self) {
        self.renew.abort();
    }
}

impl PortMap {
    /// Opens `port` (UDP) of this PC on the router. `public_ip`, when known
    /// (STUN), must match the router's: otherwise another NAT sits behind it
    /// and the opening would be useless.
    pub async fn open(lan_ip: Ipv4Addr, port: u16, public_ip: Option<Ipv4Addr>) -> Result<Self> {
        let lan = SocketAddrV4::new(lan_ip, port);
        let mut why = vec![];
        let gw = gateway(lan_ip);
        // UC_UPNP_ONLY=1: test the UPnP path on a router that also speaks NAT-PMP.
        match gw.filter(|_| std::env::var_os("UC_UPNP_ONLY").is_none()) {
            Some(gw) => match natpmp_map(lan_ip, gw, port, port, LEASE).await {
                Ok(ext_port) => {
                    let ip = natpmp_external_ip(lan_ip, gw).await?;
                    let pm = Self::new(SocketAddrV4::new(ip, ext_port), "NAT-PMP", lan, Method::NatPmp { gateway: gw });
                    return pm.checked(public_ip).await;
                }
                Err(e) => why.push(format!("NAT-PMP : {e:#}")),
            },
            None if gw.is_none() => why.push("passerelle introuvable".into()),
            None => {}
        }
        match upnp_map(lan, port, gw).await {
            Ok((ext, gw)) => Self::new(ext, "UPnP", lan, Method::Upnp(gw)).checked(public_ip).await,
            Err(e) => {
                why.push(format!("UPnP : {e:#}"));
                bail!("{}", why.join(" ; "))
            }
        }
    }

    fn new(external: SocketAddrV4, method: &'static str, lan: SocketAddrV4, how: Method) -> Self {
        let renew = tokio::spawn(renew_loop(how.clone(), lan, external.port()));
        Self { external, method, lan, how, renew }
    }

    async fn checked(self, public_ip: Option<Ipv4Addr>) -> Result<Self> {
        match public_ip {
            Some(ip) if ip != *self.external.ip() => {
                let ext = self.external;
                self.close().await;
                bail!("la box annonce l'IP {} mais l'IP publique est {ip} : une autre box est derrière (double NAT)", ext.ip())
            }
            _ => Ok(self),
        }
    }

    /// Removes the mapping (best effort).
    pub async fn close(self) {
        self.renew.abort();
        let _ = match &self.how {
            Method::NatPmp { gateway } => natpmp_map(*self.lan.ip(), *gateway, self.lan.port(), 0, 0).await.map(drop),
            Method::Upnp(gw) => gw
                .remove_port(igd_next::PortMappingProtocol::UDP, self.external.port())
                .await
                .map_err(anyhow::Error::from),
        };
    }
}

async fn renew_loop(how: Method, lan: SocketAddrV4, ext_port: u16) {
    loop {
        tokio::time::sleep(Duration::from_secs(LEASE as u64 / 2)).await;
        let r = match &how {
            Method::NatPmp { gateway } => natpmp_map(*lan.ip(), *gateway, lan.port(), ext_port, LEASE).await.map(drop),
            Method::Upnp(gw) => gw
                .add_port(igd_next::PortMappingProtocol::UDP, ext_port, SocketAddr::V4(lan), LEASE, DESCRIPTION)
                .await
                .map_err(anyhow::Error::from),
        };
        if let Err(e) = r {
            tracing::warn!("ouverture de port : renouvellement refusé par la box : {e:#}");
        }
    }
}

/// Router of the interface that reaches the Internet from `lan_ip`.
fn gateway(lan_ip: Ipv4Addr) -> Option<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
    let mut row: MIB_IPFORWARDROW = unsafe { std::mem::zeroed() };
    let dest = u32::from_ne_bytes([8, 8, 8, 8]);
    let src = u32::from_ne_bytes(lan_ip.octets());
    // SAFETY: `row` is a valid out-parameter for the call.
    if unsafe { GetBestRoute(dest, src, &mut row) } != 0 {
        return None;
    }
    let hop = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
    (!hop.is_unspecified()).then_some(hop)
}

/// One NAT-PMP request with the RFC 6886 retries (250, 500, 1000 ms).
async fn natpmp_call(lan_ip: Ipv4Addr, gw: Ipv4Addr, req: &[u8], want_op: u8) -> Result<Vec<u8>> {
    // From the LAN interface: another adapter (VPN) must not carry it.
    let sock = UdpSocket::bind((lan_ip, 0)).await?;
    sock.connect((gw, NATPMP_PORT)).await?;
    let mut buf = [0u8; 64];
    for wait in [250, 500, 1000] {
        sock.send(req).await?;
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(wait), sock.recv(&mut buf)).await {
            if n >= 8 && buf[1] == want_op {
                let code = u16::from_be_bytes([buf[2], buf[3]]);
                if code != 0 {
                    bail!("refusé par la box (code {code})");
                }
                return Ok(buf[..n].to_vec());
            }
        }
    }
    bail!("pas de réponse de la box")
}

/// Maps UDP `internal` (to `suggested` outside; 0/0 = remove): the external port.
async fn natpmp_map(lan_ip: Ipv4Addr, gw: Ipv4Addr, internal: u16, suggested: u16, lease: u32) -> Result<u16> {
    let mut req = vec![0, 1, 0, 0];
    req.extend(internal.to_be_bytes());
    req.extend(suggested.to_be_bytes());
    req.extend(lease.to_be_bytes());
    let r = natpmp_call(lan_ip, gw, &req, 129).await?;
    let ext = r.get(10..12).context("réponse NAT-PMP trop courte")?;
    Ok(u16::from_be_bytes([ext[0], ext[1]]))
}

async fn natpmp_external_ip(lan_ip: Ipv4Addr, gw: Ipv4Addr) -> Result<Ipv4Addr> {
    let r = natpmp_call(lan_ip, gw, &[0, 0], 128).await?;
    let ip = r.get(8..12).context("réponse NAT-PMP trop courte")?;
    Ok(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]))
}

/// Finds the UPnP router: our gateway asked directly first (another UPnP
/// device of the LAN, not connected to the Internet, may answer a general
/// search first), then any.
async fn upnp_gateway(lan_ip: Ipv4Addr, gw: Option<Ipv4Addr>) -> Result<(Ipv4Addr, igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>)> {
    let targets = gw.map(|g| SocketAddr::new(IpAddr::V4(g), 1900)).into_iter().chain([igd_next::SearchOptions::default().broadcast_address]);
    let mut last = anyhow::anyhow!("aucune box UPnP");
    for target in targets {
        // From the LAN interface (not a VPN's), short: the app waits for it.
        let mut opts = igd_next::SearchOptions::default();
        opts.bind_addr = SocketAddr::new(IpAddr::V4(lan_ip), 0);
        opts.broadcast_address = target;
        opts.timeout = Some(Duration::from_secs(2));
        opts.single_search_timeout = Some(Duration::from_secs(2));
        match igd_next::aio::tokio::search_gateway(opts).await {
            Ok(g) => match g.get_external_ip().await {
                Ok(IpAddr::V4(ip)) if !ip.is_unspecified() => return Ok((ip, g)),
                Ok(_) => last = anyhow::anyhow!("la box UPnP {} n'a pas d'IPv4 publique", g.addr),
                Err(e) => last = anyhow::anyhow!("box UPnP {} : {e}", g.addr),
            },
            Err(e) => last = anyhow::Error::from(e).context("aucune box UPnP"),
        }
    }
    Err(last)
}

async fn upnp_map(lan: SocketAddrV4, port: u16, gw: Option<Ipv4Addr>) -> Result<(SocketAddrV4, igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>)> {
    let (ip, gw) = upnp_gateway(*lan.ip(), gw).await?;
    let proto = igd_next::PortMappingProtocol::UDP;
    let ext = match gw.add_port(proto, port, SocketAddr::V4(lan), LEASE, DESCRIPTION).await {
        Ok(()) => port,
        // Port taken outside (another PC): any free one.
        Err(_) => gw.add_any_port(proto, SocketAddr::V4(lan), LEASE, DESCRIPTION).await?,
    };
    Ok((SocketAddrV4::new(ip, ext), gw))
}
