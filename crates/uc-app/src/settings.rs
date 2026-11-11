//! User settings, persisted in `uc-data/settings.json` next to the exe.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    // Profile
    pub name: String,
    /// "emoji|#rrggbb".
    pub avatar: String,
    /// Voice preferences (used once voice chat exists).
    pub voice_auto_join: bool,
    pub voice_start_muted: bool,
    // App
    pub autostart: bool,
    /// Last state of the main switch, restored at launch.
    pub enabled: bool,
    /// Salons (re)connected when the switch is on.
    pub connected: Vec<String>,
    /// The one connected salon with a virtual adapter (game mode).
    pub game_salon: Option<String>,
    pub relays: Vec<String>,
    /// Open EasyTier's port on the router automatically (NAT-PMP / UPnP).
    pub port_map: bool,
    /// Fixed UDP/TCP port for EasyTier (0 = automatic), for a router without
    /// UPnP where the user forwards that port by hand.
    pub custom_port: u16,
    /// Look for a new version on GitHub (at start, then every few hours).
    pub update_check: bool,
    /// Windows notifications, shown only while the window is not in front.
    pub notify_online: bool,
    pub notify_knock: bool,
    pub notify_mention: bool,
    /// Shows measurement details (live ping graph...) in the interface.
    pub debug: bool,
    /// Interface language: "" follows Windows, "fr" or "en".
    pub lang: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Joueur".into()),
            avatar: "🎮|#38d6a0".into(),
            voice_auto_join: true,
            voice_start_muted: false,
            autostart: false,
            enabled: false,
            connected: vec![],
            game_salon: None,
            relays: vec![],
            port_map: true,
            custom_port: 0,
            update_check: true,
            notify_online: true,
            notify_knock: true,
            notify_mention: true,
            debug: false,
            lang: String::new(),
        }
    }
}

pub fn data_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("uc-data")))
        .unwrap_or_else(|| "uc-data".into())
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("settings.json"), t);
        }
    }
}

/// Launch at Windows login through the per-user Run key (no admin needed).
pub fn apply_autostart(enabled: bool) -> anyhow::Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    let run = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        KEY_READ | KEY_SET_VALUE,
    )?;
    if enabled {
        let exe = std::env::current_exe()?;
        run.set_value("UberConnector", &format!("\"{}\" --minimized", exe.display()))?;
    } else {
        let _ = run.delete_value("UberConnector");
    }
    Ok(())
}

/// Makes `uberconnector://` links open this copy of the app (per user, no
/// admin). Rewritten at each start: the portable folder may have moved.
pub fn register_link_scheme() -> anyhow::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let exe = std::env::current_exe()?;
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(format!(r"Software\Classes\{}", uc_core::room::LINK_SCHEME))?;
    key.set_value("", &"URL:UberConnector")?;
    key.set_value("URL Protocol", &"")?;
    key.create_subkey("DefaultIcon")?.0.set_value("", &format!("\"{}\",0", exe.display()))?;
    key.create_subkey(r"shell\open\command")?
        .0
        .set_value("", &format!("\"{}\" \"%1\"", exe.display()))?;
    Ok(())
}

/// The invite link among command-line arguments (set by Windows on a click).
pub fn link_arg<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let prefix = format!("{}:", uc_core::room::LINK_SCHEME);
    args.into_iter().find(|a| a.to_ascii_lowercase().starts_with(&prefix))
}
