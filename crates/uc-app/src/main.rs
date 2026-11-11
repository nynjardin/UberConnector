#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod i18n;
mod notify;
mod settings;
mod updater;

use engine::{Engine, Phase, Snapshot};
use i18n::{tr, trf};
use settings::Settings;
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

type Eng<'a> = State<'a, Arc<Engine>>;

#[tauri::command]
fn get_state(engine: Eng) -> Snapshot {
    engine.snapshot()
}

#[tauri::command]
async fn set_enabled(engine: Eng<'_>, on: bool) -> Result<(), String> {
    engine.inner().clone().set_enabled(on).await;
    Ok(())
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// Returns the new salon's id (the UI selects it).
#[tauri::command]
async fn create_salon(engine: Eng<'_>, name: String) -> Result<String, String> {
    engine.inner().clone().create_salon(&name).await.map_err(err)
}

#[tauri::command]
async fn join_invite(engine: Eng<'_>, code: String) -> Result<String, String> {
    engine.inner().clone().join_invite(&code).await.map_err(err)
}

#[tauri::command]
async fn connect_salon(engine: Eng<'_>, id: String) -> Result<(), String> {
    engine.inner().clone().connect(&id).await;
    Ok(())
}

#[tauri::command]
fn disconnect(engine: Eng, id: String) {
    engine.disconnect(&id);
}

#[tauri::command]
fn set_game(engine: Eng, id: Option<String>) {
    engine.set_game(id);
}

#[tauri::command]
fn say(engine: Eng, id: String, text: String) -> Result<(), String> {
    engine.say(&id, &text).map_err(err)
}

#[tauri::command]
fn diagnostic(engine: Eng, id: String) -> String {
    engine.diagnostic(&id)
}

#[tauri::command]
fn game_test(engine: Eng, id: String) {
    engine.game_test(&id);
}

#[tauri::command]
fn get_chat(engine: Eng, id: String) -> Vec<uc_core::chat::ChatLine> {
    engine.chat(&id)
}

/// Stores a (resized) avatar image, returns its hash.
#[tauri::command]
fn save_image(engine: Eng, bytes: Vec<u8>) -> Result<String, String> {
    engine.save_image(&bytes).map_err(err)
}

#[tauri::command]
fn get_image(engine: Eng, hash: String) -> Option<String> {
    engine.image(&hash)
}

#[tauri::command]
async fn set_salon_avatar(engine: Eng<'_>, id: String, avatar: String) -> Result<(), String> {
    engine.set_salon_avatar(&id, &avatar).await.map_err(err)
}

/// Returns how many members were handed the new key.
#[tauri::command]
async fn ban_member(engine: Eng<'_>, id: String, member: String) -> Result<usize, String> {
    engine.ban(&id, &member).await.map_err(err)
}

#[tauri::command]
async fn unban_member(engine: Eng<'_>, id: String, member: String) -> Result<(), String> {
    engine.unban(&id, &member).await.map_err(err)
}

#[tauri::command]
async fn set_moderator(engine: Eng<'_>, id: String, member: String, on: bool) -> Result<(), String> {
    engine.set_moderator(&id, &member, on).await.map_err(err)
}

#[tauri::command]
async fn regenerate_invite(engine: Eng<'_>, id: String) -> Result<usize, String> {
    engine.regenerate(&id).await.map_err(err)
}

/// Returns how many members were handed the new key (when turned on).
#[tauri::command]
async fn set_gated(engine: Eng<'_>, id: String, on: bool) -> Result<usize, String> {
    engine.set_gated(&id, on).await.map_err(err)
}

#[tauri::command]
async fn admit_member(engine: Eng<'_>, id: String, member: String) -> Result<(), String> {
    engine.admit(&id, &member).await.map_err(err)
}

#[tauri::command]
fn dismiss_knock(engine: Eng, id: String, member: String) {
    engine.dismiss(&id, &member);
}

#[tauri::command]
fn forget_salon(engine: Eng, id: String) -> Result<(), String> {
    engine.forget(&id).map_err(err)
}

#[tauri::command]
async fn rename_salon(engine: Eng<'_>, id: String, name: String) -> Result<(), String> {
    engine.rename(&id, &name).await.map_err(err)
}

#[tauri::command]
async fn delete_salon(engine: Eng<'_>, id: String) -> Result<(), String> {
    engine.delete(&id).await.map_err(err)
}

/// Invite link that started (or was handed to) the app, until the UI takes it.
struct PendingLink(Mutex<Option<String>>);

/// The clicked invite's code, once (the UI asks before joining).
#[tauri::command]
fn take_invite(link: State<PendingLink>) -> Option<String> {
    let text = link.0.lock().unwrap().take()?;
    uc_core::room::InviteKeys::from_code(&text).ok().map(|k| k.code)
}

fn hand_link(app: &AppHandle, link: String) {
    *app.state::<PendingLink>().0.lock().unwrap() = Some(link);
    let _ = app.emit("invite", ());
}

/// Last newer version found on GitHub, ready to install.
struct PendingUpdate(Mutex<Option<updater::Update>>);

#[tauri::command]
fn app_version() -> &'static str {
    updater::CURRENT
}

/// Interface language, "fr" or "en" (Windows language or the setting).
#[tauri::command]
fn ui_lang() -> &'static str {
    i18n::code()
}

/// Looks for a newer version now (also done in the background).
#[tauri::command]
async fn check_update(pending: State<'_, PendingUpdate>) -> Result<Option<updater::Update>, String> {
    let found = tauri::async_runtime::spawn_blocking(updater::check).await.map_err(|e| e.to_string())?.map_err(err)?;
    *pending.0.lock().unwrap() = found.clone();
    Ok(found)
}

/// Stops the engine, swaps the files and restarts on the new version.
/// Progress goes out as "update-progress" events ([done, total] bytes).
#[tauri::command]
async fn install_update(app: AppHandle, engine: Eng<'_>, pending: State<'_, PendingUpdate>) -> Result<(), String> {
    let Some(update) = pending.0.lock().unwrap().clone() else {
        return Err(tr!("aucune mise à jour en attente", "no pending update").into());
    };
    // The engine's files (uc-helper, EasyTier) are replaced too.
    engine.shutdown();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let work = settings::data_dir().join("update");
    let app2 = app.clone();
    let exe = tauri::async_runtime::spawn_blocking(move || {
        updater::install(&update, &work, |done, total| {
            let _ = app2.emit("update-progress", (done, total));
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(err)?;
    // The new version waits for us to exit (single instance), then cleans up.
    std::process::Command::new(&exe)
        .args(["--wait-pid", &std::process::id().to_string()])
        .spawn()
        .map_err(|e| trf!("lancement de la nouvelle version : {e}", "starting the new version: {e}"))?;
    app.exit(0);
    Ok(())
}

/// Background check: a while after start, then every 6 hours.
async fn update_loop(app: AppHandle, engine: Arc<Engine>) {
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    loop {
        if engine.snapshot().settings.update_check {
            match tauri::async_runtime::spawn_blocking(updater::check).await {
                Ok(Ok(Some(u))) => {
                    *app.state::<PendingUpdate>().0.lock().unwrap() = Some(u.clone());
                    let _ = app.emit("update", u);
                }
                Ok(Err(e)) => eprintln!("mise à jour : {e:#}"),
                _ => {}
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
    }
}

/// After an update: waits for the previous version to exit (it holds the
/// single-instance lock and its files), then removes its `*.old` files.
fn after_update() {
    let mut args = std::env::args();
    let Some(pid) = args.position(|a| a == "--wait-pid").and(args.next()).and_then(|p| p.parse::<u32>().ok()) else {
        return;
    };
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: plain handle use, closed below.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 15_000);
            CloseHandle(h);
        }
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
        updater::cleanup(&dir);
    }
}

#[tauri::command]
fn save_settings(engine: Eng, settings: Settings) -> Result<(), String> {
    engine.save(settings).map_err(|e| format!("{e:#}"))
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn phase_label(s: &Snapshot) -> String {
    match (s.phase, s.settings.connected.len()) {
        (Phase::Off, _) => tr!("désactivé", "off").into(),
        (Phase::Starting, _) => tr!("activation…", "starting…").into(),
        (Phase::On, 0) => tr!("activé, aucun salon", "on, no room").into(),
        (Phase::On, 1) => tr!("connecté à 1 salon", "connected to 1 room").into(),
        (Phase::On, n) => trf!("connecté à {n} salons", "connected to {n} rooms"),
    }
}

fn setup_tray(app: &AppHandle, engine: &Arc<Engine>) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", tr!("Ouvrir UberConnector", "Open UberConnector"), true, None::<&str>)?;
    let toggle = CheckMenuItem::with_id(app, "toggle", tr!("Activé", "Enabled"), true, false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", tr!("Quitter", "Quit"), true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &toggle, &sep, &quit])?;

    let tray = TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("UberConnector")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| {
            let engine = app.state::<Arc<Engine>>().inner().clone();
            match ev.id().as_ref() {
                "open" => show_main(app),
                "toggle" => {
                    let on = engine.snapshot().phase == Phase::Off;
                    tauri::async_runtime::spawn(async move { engine.set_enabled(on).await });
                }
                "quit" => {
                    engine.shutdown();
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    let app = app.clone();
    engine.set_on_change(move |s| {
        // Menu and tray calls made from another thread WAIT for the main
        // thread. The engine calls us from its own tasks (every status of the
        // helper) while holding its observer lock, and a command running on
        // the main thread (leaving a salon…) wants that same lock: waiting
        // here froze the app for good. So the work is only queued.
        let (on, tip) = (s.phase != Phase::Off, format!("UberConnector : {}", phase_label(s)));
        let (open, toggle, quit, tray) = (open.clone(), toggle.clone(), quit.clone(), tray.clone());
        let _ = app.run_on_main_thread(move || {
            // Also follows a language change in the settings.
            let _ = open.set_text(tr!("Ouvrir UberConnector", "Open UberConnector"));
            let _ = toggle.set_text(tr!("Activé", "Enabled"));
            let _ = quit.set_text(tr!("Quitter", "Quit"));
            let _ = toggle.set_checked(on);
            let _ = tray.set_tooltip(Some(tip));
        });
    });
    Ok(())
}

fn main() {
    after_update();
    // Before any window or tray text.
    i18n::set(&Settings::load(&settings::data_dir()).lang);
    // A clicked invite link shows the window even if autostart minimized it.
    let link = settings::link_arg(std::env::args());
    let minimized = link.is_none() && std::env::args().any(|a| a == "--minimized");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            show_main(app);
            if let Some(link) = settings::link_arg(args) {
                hand_link(app, link);
            }
        }))
        .manage(PendingLink(Mutex::new(link)))
        .manage(PendingUpdate(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            take_invite,
            app_version,
            ui_lang,
            check_update,
            install_update,
            get_state,
            set_enabled,
            create_salon,
            join_invite,
            connect_salon,
            disconnect,
            set_game,
            say,
            game_test,
            diagnostic,
            get_chat,
            save_image,
            get_image,
            set_salon_avatar,
            ban_member,
            unban_member,
            set_moderator,
            regenerate_invite,
            set_gated,
            admit_member,
            dismiss_knock,
            forget_salon,
            rename_salon,
            delete_salon,
            save_settings
        ])
        .setup(move |app| {
            // Here, not before the builder: a second instance (a link
            // clicked while we run, maybe from another copy of the app)
            // exits in the single-instance plugin and must not repoint these.
            if let Err(e) = settings::register_link_scheme() {
                eprintln!("liens uberconnector:// non enregistrés : {e:#}");
            }
            if let Err(e) = notify::register(&settings::data_dir()) {
                eprintln!("notifications Windows indisponibles : {e:#}");
            }
            let handle = app.handle().clone();
            let engine = Engine::new(handle.clone())?;
            app.manage(engine.clone());
            tauri::async_runtime::spawn(engine.clone().refresh_loop());
            tauri::async_runtime::spawn(engine.clone().lobby_loop());
            tauri::async_runtime::spawn(update_loop(handle.clone(), engine.clone()));
            setup_tray(&handle, &engine)?;
            // Test: a toast at launch (click it: shows the window).
            if std::env::var_os("UC_TEST_NOTIFY").is_some() {
                notify::show(&handle, "Test UberConnector", tr!("Les notifications fonctionnent.", "Notifications work."), "");
            }
            if !minimized {
                show_main(&handle);
            }
            // Restore the last state of the main switch.
            if engine.snapshot().settings.enabled {
                tauri::async_runtime::spawn(async move { engine.set_enabled(true).await });
            }
            Ok(())
        })
        .on_window_event(|window, ev| {
            // Closing the window keeps the app (and the connection) in the tray.
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running UberConnector");
}
