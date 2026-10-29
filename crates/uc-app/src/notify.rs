//! Windows notifications (toasts). The app is portable (no installer, no
//! Start menu shortcut), so its toast identity is declared per user in the
//! registry at each start, like the `uberconnector://` link.

use anyhow::Result;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::HSTRING;

const AUMID: &str = "UberConnector.App";

/// Shown toasts, kept alive so a click still reaches us later (Action Center).
static SHOWN: Mutex<VecDeque<ToastNotification>> = Mutex::new(VecDeque::new());

/// Declares the toast identity (name + icon) and adopts it for this process.
pub fn register(data_dir: &Path) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let icon = data_dir.join("icon.png");
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(&icon, include_bytes!("../icons/icon.png"))?;
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"Software\Classes\AppUserModelId\{AUMID}"))?;
    key.set_value("DisplayName", &"UberConnector")?;
    key.set_value("IconUri", &icon.display().to_string())?;
    let id: Vec<u16> = AUMID.encode_utf16().chain([0]).collect();
    // SAFETY: NUL-terminated UTF-16 string that outlives the call.
    unsafe { windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_ptr()) };
    Ok(())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Shows a toast; a click brings the window back on `salon_id`.
pub fn show(app: &AppHandle, title: &str, body: &str, salon_id: &str) {
    let xml = format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        escape(title),
        escape(body)
    );
    let (app2, id) = (app.clone(), salon_id.to_string());
    // WinRT wants a COM-initialized thread: the UI thread is one.
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = toast(&app2, &xml, id) {
            eprintln!("notification : {e}");
        }
    });
}

fn toast(app: &AppHandle, xml: &str, salon_id: String) -> windows::core::Result<()> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    let app = app.clone();
    toast.Activated(&windows::Foundation::TypedEventHandler::new(move |_, _| {
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
        let _ = app.emit("select", &salon_id);
        Ok(())
    }))?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)?;
    let mut shown = SHOWN.lock().unwrap();
    shown.push_back(toast);
    if shown.len() > 20 {
        shown.pop_front();
    }
    Ok(())
}

/// Whether `text` names `name` (case-insensitive, as a whole word, `@` optional).
pub fn mentions(text: &str, name: &str) -> bool {
    let (text, name) = (text.to_lowercase(), name.trim().to_lowercase());
    if name.is_empty() {
        return false;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(&name).any(|(i, _)| !word(text[..i].chars().next_back()) && !word(text[i + name.len()..].chars().next()))
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn mention_words() {
        assert!(mentions("salut @Pape, tu viens ?", "pape"));
        assert!(mentions("PAPE", "Pape"));
        assert!(mentions("on attend Jean Michel", "jean michel"));
        assert!(!mentions("papeterie", "Pape"));
        assert!(!mentions("le_pape", "pape"));
        assert!(!mentions("rien", " "));
    }
}
