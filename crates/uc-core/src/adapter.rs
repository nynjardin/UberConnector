//! The one virtual network card (Wintun) shared by every game-mode salon.
//!
//! Only one salon is in game mode at a time, and a Wintun card serves one
//! EasyTier at a time, so a single card is enough. The engine (elevated)
//! creates it the first time a salon goes into game mode and keeps it until
//! it exits; Wintun removes it then. EasyTier opens a card by name before
//! creating one (rust-tun: `Adapter::open`, then `Adapter::create`), so
//! each session reuses ours and only sets its salon's IP on it.
//!
//! Creating a card is installing a device: 2 to 12 s depending on the PC,
//! and some network cards reset when it happens (seen with an Intel I218-V,
//! and with every VPN on that PC). Now it happens once per engine run
//! instead of once per session.

use anyhow::{Result, bail};
use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::core::GUID;

/// The card's name (EasyTier's `--dev-name`), as games list it.
pub const NAME: &str = "UberConnector";
/// Fixed, so Windows recognises the same network on every run.
const CARD_GUID: GUID = GUID::from_u128(0x6e5f0a3c_2b7d_4c1e_9a44_5c3b1d2e7f10);

type CreateFn = unsafe extern "system" fn(*const u16, *const u16, *const GUID) -> *mut c_void;
type CloseFn = unsafe extern "system" fn(*mut c_void);

struct Card {
    lib: HMODULE,
    handle: *mut c_void,
    close: CloseFn,
}

// SAFETY: a Wintun adapter handle may be closed from any thread.
unsafe impl Send for Card {}

impl Drop for Card {
    fn drop(&mut self) {
        // SAFETY: `handle` came from WintunCreateAdapter of this library,
        // which stays loaded until after the close.
        unsafe {
            (self.close)(self.handle);
            FreeLibrary(self.lib);
        }
    }
}

static CARD: tokio::sync::Mutex<Option<Card>> = tokio::sync::Mutex::const_new(None);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn create(dll: &Path) -> Result<Card> {
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = dll.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: plain Win32 calls; the function pointers are cast to the
    // signatures of wintun.h (WINAPI is the system ABI).
    unsafe {
        let lib = LoadLibraryW(path.as_ptr());
        if lib.is_null() {
            bail!("{} introuvable ou illisible (erreur {})", dll.display(), GetLastError());
        }
        let (Some(create), Some(close)) = (
            GetProcAddress(lib, c"WintunCreateAdapter".as_ptr().cast()),
            GetProcAddress(lib, c"WintunCloseAdapter".as_ptr().cast()),
        ) else {
            FreeLibrary(lib);
            bail!("wintun.dll sans WintunCreateAdapter");
        };
        let create: CreateFn = std::mem::transmute(create);
        let close: CloseFn = std::mem::transmute(close);
        let name = wide(NAME);
        // Also removes cards left by an engine that was killed.
        let handle = create(name.as_ptr(), name.as_ptr(), &CARD_GUID);
        if handle.is_null() {
            let err = GetLastError();
            FreeLibrary(lib);
            bail!("création de la carte refusée par Wintun (erreur {err})");
        }
        Ok(Card { lib, handle, close })
    }
}

/// Makes sure our card exists (EasyTier's folder holds wintun.dll).
/// Returns whether it was created now; waits while another session creates it.
pub async fn ensure(et_dir: &Path) -> Result<bool> {
    let mut card = CARD.lock().await;
    if card.is_some() {
        return Ok(false);
    }
    let dll = et_dir.join("wintun.dll");
    *card = Some(tokio::task::spawn_blocking(move || create(&dll)).await??);
    Ok(true)
}

/// Removes the card (engine exit). Skipped while it is being created: the
/// process is ending anyway, and Wintun removes what a dead process left.
pub fn release() {
    if let Ok(mut card) = CARD.try_lock() {
        card.take();
    }
}

/// Removes the IPv4 addresses left on our card. EasyTier sets its salon's
/// address when it starts but never removes it, and fails to start when the
/// address is already there: seen on 2026-11-14 when the game salon came
/// back to a salon used earlier, with both salons' addresses on the card.
/// Called before and after each game-mode EasyTier. Returns how many went.
pub fn clear_addresses() -> Result<usize> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        ConvertInterfaceAliasToLuid, DeleteUnicastIpAddressEntry, FreeMibTable, GetUnicastIpAddressTable,
        MIB_UNICASTIPADDRESS_TABLE,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    let name = wide(NAME);
    // SAFETY: Win32 calls with valid out-pointers; the table is read within
    // its NumEntries and freed once.
    unsafe {
        let mut luid: NET_LUID_LH = std::mem::zeroed();
        if ConvertInterfaceAliasToLuid(name.as_ptr(), &mut luid) != 0 {
            return Ok(0); // no card (yet)
        }
        let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
        let err = GetUnicastIpAddressTable(AF_INET, &mut table);
        if err != 0 {
            bail!("liste des adresses illisible (erreur {err})");
        }
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let mut removed = 0;
        for row in rows.iter().filter(|r| r.InterfaceLuid.Value == luid.Value) {
            if DeleteUnicastIpAddressEntry(row) == 0 {
                removed += 1;
            }
        }
        FreeMibTable(table.cast());
        Ok(removed)
    }
}

/// One of our cards: the shared one, or a per-salon one of versions <= 0.2.4.
pub fn is_ours(alias: &str) -> bool {
    alias == NAME || alias.starts_with("UC-")
}
