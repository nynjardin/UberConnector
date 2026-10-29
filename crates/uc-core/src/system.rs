//! Windows helpers: admin check and temporary firewall rule.

use anyhow::{Result, bail};
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

fn netsh(args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let out = Command::new("netsh")
        .args(["advfirewall", "firewall"])
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if !out.status.success() {
        bail!("netsh {:?}: {}", args, oem_text(&out.stdout).trim());
    }
    Ok(())
}

/// Console programs (netsh...) write in the OEM code page, not UTF-8.
fn oem_text(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MultiByteToWideChar};
    if bytes.is_empty() {
        return String::new();
    }
    let mut wide = vec![0u16; bytes.len()];
    let n = unsafe {
        MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), wide.len() as i32)
    };
    if n <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..n as usize])
}

/// Other network adapters Windows may send a 255.255.255.255 broadcast
/// through instead of ours (`own`, e.g. "UC-75349b"): connected ones with a
/// metric as good as ours (Radmin VPN, ZeroTier...), except the one leading to
/// the Internet, whose broadcasts EasyTier relays.
pub fn broadcast_rivals(own: &str) -> Vec<String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        ConvertInterfaceLuidToAlias, FreeMibTable, GetBestInterface, GetIpInterfaceTable, MIB_IPINTERFACE_TABLE,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    let mut internet = 0u32;
    unsafe { GetBestInterface(u32::from_ne_bytes([8, 8, 8, 8]), &mut internet) };
    let mut table: *mut MIB_IPINTERFACE_TABLE = std::ptr::null_mut();
    if unsafe { GetIpInterfaceTable(AF_INET, &mut table) } != 0 || table.is_null() {
        return vec![];
    }
    let rows = unsafe { std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
    let adapters: Vec<(String, u32, u32, bool)> = rows
        .iter()
        .map(|r| {
            let mut buf = [0u16; 257];
            let ok = unsafe { ConvertInterfaceLuidToAlias(&r.InterfaceLuid, buf.as_mut_ptr(), buf.len()) } == 0;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let alias = if ok { String::from_utf16_lossy(&buf[..len]) } else { String::new() };
            (alias, r.Metric, r.InterfaceIndex, r.Connected)
        })
        .collect();
    unsafe { FreeMibTable(table as *const _) };
    let Some(ours) = adapters.iter().find(|a| a.0 == own).map(|a| a.1) else { return vec![] };
    adapters
        .into_iter()
        .filter(|(alias, metric, index, connected)| {
            *connected && *metric <= ours && alias != own && *index != internet && !alias.starts_with("Loopback")
        })
        .map(|a| a.0)
        .collect()
}

/// Name of a local interface (not one of ours) already in 10.<subnet>.0.0/16.
pub fn subnet_in_use(subnet: u8) -> Option<String> {
    if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| match i.ip() {
        std::net::IpAddr::V4(ip)
            if ip.octets()[0] == 10 && ip.octets()[1] == subnet && !i.name.starts_with("UC-") =>
        {
            Some(i.name)
        }
        _ => None,
    })
}

/// EasyTier loads the WinDivert driver (broadcast relay) but never unloads it,
/// which keeps `WinDivert64.sys` locked until a real reboot. Stop it, but only
/// when the loaded driver is ours (other software may use WinDivert too).
pub fn unload_our_windivert(easytier_dir: &std::path::Path) {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SYSTEM\CurrentControlSet\Services\WinDivert") else {
        return;
    };
    let Ok(image): Result<String, _> = key.get_value("ImagePath") else { return };
    let ours = easytier_dir.join("WinDivert64.sys").to_string_lossy().to_lowercase();
    if !image.to_lowercase().ends_with(&ours) {
        return;
    }
    use std::os::windows::process::CommandExt;
    let _ = Command::new("sc").args(["stop", "WinDivert"]).creation_flags(CREATE_NO_WINDOW).output();
}

/// Lets the engine itself receive packets (hole punching). Windows' prompt
/// for a hidden elevated process is easily missed and then leaves a *block*
/// rule for its exe, which beats any allow rule: replace whatever rules
/// exist for our exe with one allow rule, on every profile.
pub fn allow_this_program() -> Result<()> {
    let exe = std::env::current_exe()?;
    let program = format!("program={}", exe.display());
    let _ = netsh(&["delete", "rule", "name=all", &program]);
    netsh(&["add", "rule", "name=UberConnector (moteur)", "dir=in", "action=allow", "profile=any", &program, "enable=yes"])
}

/// Allows every inbound packet coming from the room subnet, removed on drop.
/// Without it Windows usually drops game traffic on the virtual adapter.
pub struct FirewallRule {
    name: String,
}

impl FirewallRule {
    pub fn allow_subnet(subnet_cidr: &str) -> Result<Self> {
        let name = format!("UberConnector ({subnet_cidr})");
        let _ = netsh(&["delete", "rule", &format!("name={name}")]);
        for proto in ["any", "icmpv4:any,any"] {
            netsh(&[
                "add",
                "rule",
                &format!("name={name}"),
                "dir=in",
                "action=allow",
                "profile=any",
                &format!("protocol={proto}"),
                &format!("remoteip={subnet_cidr}"),
            ])?;
        }
        Ok(Self { name })
    }
}

impl Drop for FirewallRule {
    fn drop(&mut self) {
        let _ = netsh(&["delete", "rule", &format!("name={}", self.name)]);
    }
}
