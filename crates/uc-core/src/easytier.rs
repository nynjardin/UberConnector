//! Drives the bundled `easytier-core` / `easytier-cli` executables.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Looks for the EasyTier binaries next to the exe (`easytier/`), then in
/// `vendor/easytier/easytier-windows-x86_64` of any parent (dev builds).
pub fn find_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("UC_EASYTIER_DIR") {
        return Ok(d.into());
    }
    let exe = std::env::current_exe()?;
    let mut dir = exe.parent();
    while let Some(d) = dir {
        for cand in [d.join("easytier"), d.join("vendor/easytier/easytier-windows-x86_64")] {
            if cand.join("easytier-core.exe").exists() {
                return Ok(cand);
            }
        }
        dir = d.parent();
    }
    bail!("easytier-core.exe introuvable (dossier `easytier` à côté de l'exécutable)")
}

/// `UC_NO_TUN=1`: no virtual adapter, no admin needed (local testing only).
pub fn test_no_tun() -> bool {
    std::env::var_os("UC_NO_TUN").is_some()
}

pub struct CoreConfig<'a> {
    pub network_name: &'a str,
    pub network_secret: &'a str,
    pub ipv4_cidr: String,
    pub hostname: &'a str,
    /// Virtual adapter name, unique per salon.
    pub dev_name: String,
    /// Game mode: create the virtual adapter (full LAN). Otherwise no adapter
    /// and only `open_ports` are reachable by other members.
    pub tun: bool,
    pub open_tcp_ports: Vec<u16>,
    /// Local SOCKS5 proxy into the salon network (chat/voice transport).
    pub socks5_port: u16,
    pub machine_id: String,
    pub listen_port: u16,
    pub rpc_port: u16,
    pub relays: &'a [String],
    pub log_dir: &'a Path,
}

pub struct EasyTier {
    dir: PathBuf,
    rpc: String,
    child: Child,
    _job: job::Job,
}

impl EasyTier {
    pub fn spawn(dir: &Path, cfg: &CoreConfig) -> Result<Self> {
        std::fs::create_dir_all(cfg.log_dir)?;
        let mut cmd = Command::new(dir.join("easytier-core.exe"));
        cmd.current_dir(dir)
            .args(["--network-name", cfg.network_name])
            // Not on the command line: any local process can read that of
            // another process, even an elevated one, but not its environment.
            .env("ET_NETWORK_SECRET", cfg.network_secret)
            .args(["-i", &cfg.ipv4_cidr])
            .args(["--hostname", cfg.hostname])
            .args(["--machine-id", &cfg.machine_id])
            .args(["--dev-name", &cfg.dev_name])
            .arg("-l")
            .arg(format!("udp://0.0.0.0:{}", cfg.listen_port))
            .arg(format!("tcp://0.0.0.0:{}", cfg.listen_port))
            .args(["-r", &format!("127.0.0.1:{}", cfg.rpc_port)])
            .args(["--console-log-level", "warn"])
            // "info" logs every unroutable multicast packet (mDNS...): ~1 MB/min.
            .args(["--file-log-level", "warn"])
            .arg("--file-log-dir")
            .arg(cfg.log_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // A crash (panic) only shows up there. Appended, so a restart
            // keeps the previous crash; started over once it gets big.
            .stderr(stderr_log(cfg.log_dir).map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true);
        cmd.args(["--socks5", &cfg.socks5_port.to_string()]);
        // Game announcements to 255.255.255.255 are relayed by us (see
        // `bcast`), not by EasyTier's relay (WinDivert driver): it misses those
        // leaving through another VPN's adapter or ours.
        if !cfg.tun || test_no_tun() {
            // Without adapter EasyTier forwards inbound connections to local
            // ports: only expose ours. Port 0 closes UDP entirely.
            let tcp: Vec<String> = cfg.open_tcp_ports.iter().map(|p| p.to_string()).collect();
            cmd.arg("--no-tun").arg("--tcp-whitelist").args(&tcp).args(["--udp-whitelist", "0"]);
        }
        // The list comes from a user-writable file and we run elevated: an
        // entry must never be read as an option of easytier-core.
        let relays: Vec<&str> = cfg.relays.iter().map(|r| r.trim()).filter(|r| !r.is_empty() && !r.starts_with('-')).collect();
        if !relays.is_empty() {
            cmd.arg("-p").args(relays);
        }
        let child = cmd.spawn().context("lancement de easytier-core")?;
        let job = job::Job::kill_on_close()?;
        if let Some(h) = child.raw_handle() {
            job.assign(h)?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            rpc: format!("127.0.0.1:{}", cfg.rpc_port),
            child,
            _job: job,
        })
    }

    async fn cli(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(self.dir.join("easytier-cli.exe"))
            .args(["-p", &self.rpc, "-o", "json"])
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .output();
        // Called from the session loop: a stuck CLI must not freeze it.
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), out)
            .await
            .context("easytier-cli ne répond pas")??;
        if !out.status.success() {
            bail!("easytier-cli {:?}: {}", args, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub async fn peers(&self) -> Result<Vec<EtPeer>> {
        Ok(serde_json::from_str(&self.cli(&["peer", "list"]).await?)?)
    }

    pub async fn routes(&self) -> Result<Vec<EtRoute>> {
        Ok(serde_json::from_str(&self.cli(&["route"]).await?)?)
    }

    pub async fn add_connector(&self, url: &str) -> Result<()> {
        self.cli(&["connector", "add", url]).await.map(|_| ())
    }

    pub async fn remove_connector(&self, url: &str) -> Result<()> {
        self.cli(&["connector", "remove", url]).await.map(|_| ())
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct EtPeer {
    pub ipv4: String,
    pub hostname: String,
    /// "Local", "p2p" or "relay(n)".
    pub cost: String,
    pub lat_ms: String,
    pub loss_rate: String,
    pub tunnel_proto: String,
    pub nat_type: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EtRoute {
    /// "<name>~<id>" of the destination.
    pub hostname: String,
    /// "Local", "DIRECT", or the relaying member's "<name>~<id>".
    pub next_hop_hostname: String,
    #[serde(default)]
    pub path_len: u32,
}

/// Returns a free local port usable for both TCP and UDP.
const STDERR_LOG: &str = "easytier-stderr.log";
const STDERR_MAX: u64 = 1 << 20;

fn stderr_log(log_dir: &Path) -> std::io::Result<std::fs::File> {
    let path = log_dir.join(STDERR_LOG);
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > STDERR_MAX);
    std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(path)
}

/// Last lines of EasyTier's error output that explain a crash (panic or
/// fatal error), without the usual noise.
pub fn crash_cause(log_dir: &Path) -> Option<String> {
    let text = std::fs::read(log_dir.join(STDERR_LOG)).ok()?;
    let text = String::from_utf8_lossy(&text[text.len().saturating_sub(64 * 1024)..]).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().rposition(|l| l.contains("panicked") || l.contains("FATAL") || l.contains("Error:"))?;
    Some(lines[at..(at + 4).min(lines.len())].join(" | ").chars().take(400).collect())
}

pub fn free_port() -> Result<u16> {
    for _ in 0..20 {
        let udp = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let port = udp.local_addr()?.port();
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("aucun port libre")
}

mod job {
    //! Windows Job object: EasyTier dies with us, even on a crash.
    use anyhow::{Result, bail};
    use std::os::windows::io::RawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;

    pub struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn kill_on_close() -> Result<Self> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    bail!("CreateJobObjectW");
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    bail!("SetInformationJobObject");
                }
                Ok(Self(h))
            }
        }

        pub fn assign(&self, process: RawHandle) -> Result<()> {
            if unsafe { AssignProcessToJobObject(self.0, process as HANDLE) } == 0 {
                bail!("AssignProcessToJobObject");
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}
