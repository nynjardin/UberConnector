//! `uc-helper helper --port P --token T`: elevated engine driven by the app.
//! Runs one session per connected salon. Exits (and tears everything down)
//! when the app's connection closes.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{mpsc, watch};
use uc_core::ipc::{Command, Event};
use uc_core::session::{Profile, Session, SessionConfig};

async fn send(w: &mut OwnedWriteHalf, ev: &Event) -> Result<()> {
    let mut line = serde_json::to_vec(ev)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

async fn stop(sessions: &mut HashMap<String, Session>, id: &str) {
    if let Some(s) = sessions.remove(id) {
        let _ = s.stop().await;
    }
    // EasyTier leaves WinDivert loaded; unload it once no session needs it.
    if sessions.is_empty() {
        if let Ok(dir) = uc_core::easytier::find_dir() {
            uc_core::system::unload_our_windivert(&dir);
        }
    }
}

pub async fn run(port: u16, token: String, data_dir: PathBuf) -> Result<()> {
    // Elevated (the test mode without adapter is not): make sure Windows'
    // firewall lets hole punching packets reach us.
    if uc_core::system::is_admin() {
        match uc_core::system::allow_this_program() {
            Ok(()) => tracing::info!("pare-feu : moteur autorisé"),
            Err(e) => tracing::warn!("pare-feu : impossible d'autoriser le moteur : {e:#}"),
        }
    }
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (r, mut w) = stream.into_split();
    send(&mut w, &Event::Hello { token }).await?;
    let mut lines = BufReader::new(r).lines();
    let mut sessions: HashMap<String, Session> = HashMap::new();
    let (profile_tx, profile_rx) = watch::channel(Profile::default());
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            Some(ev) = events_rx.recv() => send(&mut w, &ev).await?,
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let cmd = match serde_json::from_str::<Command>(&line) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("commande invalide : {e}");
                        continue;
                    }
                };
                match cmd {
                    Command::Profile(p) => {
                        profile_tx.send_if_modified(|cur| {
                            let changed = *cur != p;
                            *cur = p;
                            changed
                        });
                    }
                    Command::Say { salon_id, text } => {
                        if let Some(s) = sessions.get(&salon_id) {
                            s.say(text);
                        }
                    }
                    Command::WantImage { hash } => {
                        for s in sessions.values() {
                            s.want_image(hash.clone());
                        }
                    }
                    Command::NotifySalonChanged { salon_id } => {
                        if let Some(s) = sessions.get(&salon_id) {
                            s.notify_salon_changed();
                        }
                    }
                    Command::GameTest { salon_id } => {
                        if let Some(s) = sessions.get(&salon_id) {
                            s.game_test();
                        }
                    }
                    Command::SetBanned { salon_id, banned } => {
                        if let Some(s) = sessions.get(&salon_id) {
                            s.set_banned(banned);
                        }
                    }
                    Command::Join { salon_id, access_key, relays, game, banned, port_map, custom_port } => {
                        stop(&mut sessions, &salon_id).await;
                        let mut key = [0u8; 32];
                        if hex::decode_to_slice(&access_key, &mut key).is_err() {
                            let message = "clé de salon invalide".into();
                            send(&mut w, &Event::Error { salon_id: Some(salon_id), message }).await?;
                            continue;
                        }
                        let session = Session::start(SessionConfig {
                            subnet: uc_core::salons::subnet_for(&salon_id),
                            salon_id: salon_id.clone(),
                            access_key: key,
                            profile: profile_rx.clone(),
                            game,
                            banned,
                            events_out: events_tx.clone(),
                            relays,
                            fixed_ip: None,
                            use_lan: true,
                            port_map,
                            custom_port,
                            data_dir: data_dir.clone(),
                        });
                        sessions.insert(salon_id, session);
                    }
                    Command::Leave { salon_id } => {
                        stop(&mut sessions, &salon_id).await;
                        send(&mut w, &Event::Left { salon_id }).await?;
                    }
                    Command::Quit => break,
                }
            }
            _ = tick.tick() => {
                let ids: Vec<String> = sessions.keys().cloned().collect();
                for id in ids {
                    let (status, finished) = {
                        let s = &sessions[&id];
                        (s.status.borrow().clone(), s.is_finished())
                    };
                    send(&mut w, &Event::Status(status)).await?;
                    if finished {
                        let r = sessions.remove(&id).unwrap().stop().await;
                        stop(&mut sessions, &id).await;
                        let message = match r {
                            Err(e) => format!("{e:#}"),
                            Ok(()) => "session terminée".into(),
                        };
                        send(&mut w, &Event::Error { salon_id: Some(id), message }).await?;
                    }
                }
            }
        }
    }
    let ids: Vec<String> = sessions.keys().cloned().collect();
    for id in ids {
        stop(&mut sessions, &id).await;
    }
    Ok(())
}
