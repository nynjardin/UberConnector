//! App state: salons (DHT directory, non-elevated) + the elevated
//! `uc-helper.exe` (UAC prompt on activation, loopback JSON-lines channel,
//! see `uc_core::ipc`). Several salons can be connected at once.

use crate::i18n::{tr, trf};
use crate::settings::{self, Settings};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{OnceCell, mpsc};
use uc_core::chat::ChatLine;
use uc_core::identity::Identity;
use uc_core::ipc::{Command, Event};
use uc_core::salons::{self, Directory, Knock, Refresh, Salon};
use uc_core::session::{Profile, Status};

/// Salons must be republished well within the ~2 h BEP44 expiry.
const REFRESH_EVERY: Duration = Duration::from_secs(20 * 60);
/// Chat lines kept in memory per salon while connected.
const MAX_CHAT_LINES: usize = 500;
/// An identity first met less than this ago gets the "noob" badge.
const NOOB_SECS: u64 = 3 * 24 * 3600;
/// Waiting rooms: newcomers look for their admission, moderators for newcomers.
const LOBBY_EVERY: Duration = Duration::from_secs(20);
/// Newcomers rewrite their knock every this many lobby rounds.
const KNOCK_ROUNDS: u32 = 3;
/// No "player online" notification while our own connection is this young
/// (everyone already there shows up then)...
const ONLINE_QUIET: Duration = Duration::from_secs(60);
/// ...nor for a player back after a shorter absence (flapping link, game mode).
const ONLINE_GONE: Duration = Duration::from_secs(5 * 60);

#[derive(Serialize, Clone)]
struct ChatEvent {
    salon_id: String,
    line: ChatLine,
}

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Off,
    /// Waiting for the UAC prompt / helper connection.
    Starting,
    On,
}

#[derive(Serialize, Clone)]
pub struct SalonView {
    pub id: String,
    pub name: String,
    pub invite: String,
    pub is_owner: bool,
    /// Owner or moderator.
    pub can_moderate: bool,
    pub owner_id: String,
    /// Moderators' member ids.
    pub mod_ids: Vec<String>,
    pub banned: Vec<BannedView>,
    pub deleted: bool,
    pub key_lost: bool,
    pub subnet: u8,
    pub avatar: String,
    /// Newcomers must be admitted.
    pub gated: bool,
    /// We wait for admission.
    pub pending: bool,
    /// Newcomers waiting (owner and moderators only).
    pub knocks: Vec<KnockView>,
}

#[derive(Serialize, Clone)]
pub struct KnockView {
    pub id: String,
    pub name: String,
    pub avatar: String,
}

#[derive(Serialize, Clone)]
pub struct BannedView {
    pub id: String,
    /// From the roster, if we met them.
    pub name: String,
}

/// What we know of a member from all our salons (anti-abuse hints).
#[derive(Serialize, Clone)]
pub struct PersonView {
    /// Salons of ours that banned this member.
    pub banned_in: Vec<SalonRef>,
    /// Identity first met recently (or never in the chat yet).
    pub noob: bool,
}

#[derive(Serialize, Clone)]
pub struct SalonRef {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Clone)]
pub struct Snapshot {
    pub settings: Settings,
    pub phase: Phase,
    pub salons: Vec<SalonView>,
    /// Live status of each connected salon, by salon id.
    pub statuses: HashMap<String, Status>,
    /// Anti-abuse hints for every member shown in a connected salon, by member id.
    pub people: HashMap<String, PersonView>,
    pub error: Option<String>,
    /// Long operation in progress (DHT lookup...), shown to the user.
    pub busy: Option<String>,
}

struct Inner {
    settings: Settings,
    salons: Vec<Salon>,
    phase: Phase,
    statuses: HashMap<String, Status>,
    /// Chat of each connected salon, in memory only (no history).
    chats: HashMap<String, Vec<ChatLine>>,
    error: Option<String>,
    busy: Option<String>,
    tx: Option<mpsc::UnboundedSender<Command>>,
    generation: u64,
    /// Member id -> unix time we first met them in a chat (`uc-data/seen.json`).
    seen: HashMap<String, u64>,
    /// Newcomers waiting in the gated salons we moderate, by salon id.
    knocks: HashMap<String, Vec<Knock>>,
    /// (salon, member) admitted or ignored by us: no longer listed.
    knocks_done: HashSet<(String, String)>,
    /// (salon, member) knocks already announced (notice + notification).
    knocks_announced: HashSet<(String, String)>,
    /// First status of the current session, by salon id.
    status_since: HashMap<String, Instant>,
    /// Salon id -> member id -> last time listed (arrival notifications).
    last_listed: HashMap<String, HashMap<String, Instant>>,
}

impl Inner {
    fn send(&self, cmd: Command) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    fn profile(&self) -> Command {
        Command::Profile(Profile { name: self.settings.name.clone(), avatar: self.settings.avatar.clone() })
    }

    fn join(&self, salon_id: &str) -> Option<Command> {
        let s = self.salons.iter().find(|s| s.id == salon_id && !s.deleted && !s.pending)?;
        Some(Command::Join {
            salon_id: s.id.clone(),
            access_key: s.access_key.clone(),
            relays: self.settings.relays.clone(),
            port_map: self.settings.port_map,
            custom_port: self.settings.custom_port,
            game: self.settings.game_salon.as_deref() == Some(salon_id),
            banned: s.banned.clone(),
        })
    }

    /// (Re)sends Join for a connected salon, e.g. after a game mode change.
    fn rejoin(&self, salon_id: &str) {
        if self.phase == Phase::On && self.settings.connected.iter().any(|c| c == salon_id) {
            if let Some(cmd) = self.join(salon_id) {
                self.send(cmd);
            }
        }
    }

    fn salon_name(&self, id: &str) -> String {
        self.salons.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_else(|| id.into())
    }
}

pub struct Engine {
    app: AppHandle,
    data_dir: PathBuf,
    me: Identity,
    dir: OnceCell<Arc<Directory>>,
    inner: Mutex<Inner>,
    on_change: Mutex<Option<Box<dyn Fn(&Snapshot) + Send>>>,
    /// Held by whatever reads a salon, works on the DHT for seconds, then
    /// writes the salon back (refresh, owner edits, moderation): two of
    /// them at once would overwrite each other's result.
    edits: tokio::sync::Mutex<()>,
    /// The salon list changed (Nostr topics to listen to).
    salons_changed: tokio::sync::Notify,
    /// Salons being refetched after a pushed change (one at a time each).
    refreshing: Mutex<HashSet<String>>,
}

impl Engine {
    pub fn new(app: AppHandle) -> Result<Arc<Self>> {
        let data_dir = settings::data_dir();
        let me = Identity::load_or_create(&data_dir)?;
        let salons = salons::load(&data_dir);
        let seen = load_seen(&data_dir, &salons);
        Ok(Arc::new(Self {
            app,
            inner: Mutex::new(Inner {
                settings: Settings::load(&data_dir),
                salons,
                phase: Phase::Off,
                statuses: HashMap::new(),
                chats: HashMap::new(),
                error: None,
                busy: None,
                tx: None,
                generation: 0,
                seen,
                knocks: HashMap::new(),
                knocks_done: HashSet::new(),
                knocks_announced: HashSet::new(),
                status_since: HashMap::new(),
                last_listed: HashMap::new(),
            }),
            data_dir,
            me,
            dir: OnceCell::new(),
            on_change: Mutex::new(None),
            edits: Default::default(),
            salons_changed: Default::default(),
            refreshing: Default::default(),
        }))
    }

    /// Extra observer (tray menu) called on every state change.
    pub fn set_on_change(&self, f: impl Fn(&Snapshot) + Send + 'static) {
        *self.on_change.lock().unwrap() = Some(Box::new(f));
    }

    pub fn snapshot(&self) -> Snapshot {
        let i = self.inner.lock().unwrap();
        let me = self.me.public_key_hex();
        Snapshot {
            settings: i.settings.clone(),
            phase: i.phase,
            salons: i
                .salons
                .iter()
                // Owners keep deleted salons to republish the tombstone; hide them.
                .filter(|s| !(s.deleted && s.owner == me))
                .map(|s| SalonView {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    invite: s.invite.clone(),
                    is_owner: s.owner == me,
                    can_moderate: s.can_moderate(&self.me),
                    owner_id: s.owner.get(..16).unwrap_or_default().to_string(),
                    mod_ids: s.mods.iter().filter_map(|pk| pk.get(..16)).map(str::to_string).collect(),
                    banned: s
                        .banned
                        .iter()
                        .map(|id| BannedView {
                            id: id.clone(),
                            name: s.roster.iter().find(|m| m.id == *id).map(|m| m.name.clone()).unwrap_or_default(),
                        })
                        .collect(),
                    deleted: s.deleted,
                    key_lost: s.key_lost,
                    subnet: s.subnet(),
                    avatar: s.avatar.clone(),
                    gated: s.gated,
                    pending: s.pending,
                    knocks: i
                        .knocks
                        .get(&s.id)
                        .into_iter()
                        .flatten()
                        .filter(|k| !i.knocks_done.contains(&(s.id.clone(), k.id.clone())))
                        .map(|k| KnockView { id: k.id.clone(), name: k.name.clone(), avatar: k.avatar.clone() })
                        .collect(),
                })
                .collect(),
            statuses: i.statuses.clone(),
            people: people(&i, &self.me.id),
            error: i.error.clone(),
            busy: i.busy.clone(),
        }
    }

    fn changed(&self) {
        let snap = self.snapshot();
        if let Some(f) = &*self.on_change.lock().unwrap() {
            f(&snap);
        }
        let _ = self.app.emit("state", snap);
    }

    fn update<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let r = {
            let mut i = self.inner.lock().unwrap();
            f(&mut i)
        };
        self.changed();
        r
    }

    /// Windows notification, if enabled and the window is not in front.
    fn notify(&self, enabled: impl Fn(&Settings) -> bool, salon_id: &str, title: &str, body: &str) {
        if !enabled(&self.inner.lock().unwrap().settings) {
            return;
        }
        let front = self
            .app
            .get_webview_window("main")
            .is_some_and(|w| w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false));
        if !front {
            crate::notify::show(&self.app, title, body, salon_id);
        }
    }

    fn persist(&self) {
        let i = self.inner.lock().unwrap();
        i.settings.save(&self.data_dir);
        let _ = salons::save(&self.data_dir, &i.salons);
        self.salons_changed.notify_one();
    }

    async fn directory(&self) -> Result<Arc<Directory>> {
        self.dir
            .get_or_try_init(|| async { Directory::start_in(&self.data_dir).await.map(Arc::new) })
            .await
            .cloned()
    }

    /// Runs a DHT operation with a "busy" message and error reporting.
    async fn with_busy<T>(&self, msg: &str, fut: impl Future<Output = Result<T>>) -> Result<T> {
        self.update(|i| {
            i.busy = Some(msg.to_string());
            i.error = None;
        });
        let r = fut.await;
        self.update(|i| {
            i.busy = None;
            if let Err(e) = &r {
                i.error = Some(format!("{e:#}"));
            }
        });
        r
    }

    fn upsert_salon(&self, salon: Salon) {
        self.update(|i| {
            i.salons.retain(|s| s.id != salon.id);
            i.salons.insert(0, salon);
        });
        self.persist();
    }

    // ---- Switch ----

    pub async fn set_enabled(self: &Arc<Self>, on: bool) {
        if !on {
            // `settings.connected` is kept: those salons reconnect next time.
            self.update(|i| {
                i.settings.enabled = false;
                i.send(Command::Quit);
                i.tx = None;
                i.phase = Phase::Off;
                i.statuses.clear();
                i.chats.clear();
                i.error = None;
            });
            self.persist();
            return;
        }

        let Some(generation) = self.update(|i| {
            if i.phase != Phase::Off {
                return None;
            }
            i.phase = Phase::Starting;
            i.error = None;
            i.generation += 1;
            Some(i.generation)
        }) else {
            return;
        };

        let started = self.start_helper(generation).await;
        // Returns the helper channel of a stale attempt: switched off (or off
        // then on again) during the UAC prompt, its helper must not take over.
        let stale = self.update(|i| {
            if i.generation != generation || i.phase != Phase::Starting {
                return Some(started.ok());
            }
            match started {
                Ok(tx) => {
                    i.settings.enabled = true;
                    i.phase = Phase::On;
                    i.tx = Some(tx);
                    i.send(i.profile());
                    let salons = i.settings.connected.clone();
                    i.settings.connected = salons.into_iter().filter(|id| i.join(id).is_some()).collect();
                    for id in &i.settings.connected {
                        i.send(i.join(id).unwrap());
                    }
                }
                Err(e) => {
                    i.settings.enabled = false;
                    i.phase = Phase::Off;
                    i.error = Some(format!("{e:#}"));
                }
            }
            None
        });
        match stale {
            Some(tx) => {
                if let Some(tx) = tx {
                    let _ = tx.send(Command::Quit);
                }
            }
            None => {
                self.persist();
                let ids = self.inner.lock().unwrap().settings.connected.clone();
                self.refresh_on_connect(ids);
            }
        }
    }

    // ---- Salons ----

    pub async fn create_salon(self: &Arc<Self>, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!(tr!("donnez un nom au salon", "give the room a name"));
        }
        let salon = salons::create(&self.me, name);
        self.with_busy(tr!("Publication du salon…", "Publishing the room…"), async {
            self.directory().await?.publish(&self.me, &salon).await
        })
        .await?;
        let id = salon.id.clone();
        self.upsert_salon(salon);
        self.connect(&id).await;
        Ok(id)
    }

    pub async fn join_invite(self: &Arc<Self>, code: &str) -> Result<String> {
        let salon = self
            .with_busy(tr!("Recherche de l'invitation…", "Looking up the invitation…"), async {
                self.directory().await?.resolve(&self.me, code).await
            })
            .await?;
        let (id, pending) = (salon.id.clone(), salon.pending);
        self.upsert_salon(salon);
        if pending {
            // Knock right away instead of at the next lobby round.
            let me = self.clone();
            let id2 = id.clone();
            tauri::async_runtime::spawn(async move { me.pending_round(&id2, true).await });
        } else {
            self.connect(&id).await;
        }
        Ok(id)
    }

    pub async fn connect(self: &Arc<Self>, id: &str) {
        if self.inner.lock().unwrap().salons.iter().any(|s| s.id == id && s.pending) {
            return;
        }
        let phase = self.update(|i| {
            i.error = None;
            if !i.settings.connected.iter().any(|c| c == id) {
                i.settings.connected.push(id.to_string());
            }
            // Without a game salon yet, the salon we connect to becomes it.
            let game_ok = i.settings.game_salon.as_ref().is_some_and(|g| i.settings.connected.contains(g));
            if !game_ok {
                i.settings.game_salon = Some(id.to_string());
            }
            if i.phase == Phase::On {
                if let Some(cmd) = i.join(id) {
                    i.send(cmd);
                }
            }
            i.phase
        });
        self.persist();
        // Connecting while disabled activates (salons are joined once the
        // helper is up, and refetched then).
        if phase == Phase::Off {
            self.set_enabled(true).await;
        } else {
            self.refresh_on_connect(vec![id.to_string()]);
        }
    }

    pub fn disconnect(&self, id: &str) {
        self.update(|i| {
            i.send(Command::Leave { salon_id: id.to_string() });
            i.settings.connected.retain(|c| c != id);
            i.statuses.remove(id);
            i.chats.remove(id);
            if i.settings.game_salon.as_deref() == Some(id) {
                i.settings.game_salon = None;
            }
        });
        self.persist();
    }

    /// Moves the (single) virtual adapter to `id`, or removes it (`None`).
    pub fn set_game(&self, id: Option<String>) {
        self.update(|i| {
            let old = std::mem::replace(&mut i.settings.game_salon, id.clone());
            if old == id {
                return;
            }
            for s in old.iter().chain(id.iter()) {
                i.rejoin(s);
            }
        });
        self.persist();
    }

    pub fn say(&self, id: &str, text: &str) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        if text.chars().count() > uc_core::chat::MAX_TEXT {
            bail!(trf!("message trop long ({} caractères maximum)", "message too long ({} characters maximum)", uc_core::chat::MAX_TEXT));
        }
        self.inner.lock().unwrap().send(Command::Say { salon_id: id.to_string(), text: text.to_string() });
        Ok(())
    }

    /// Text report to paste when asking for help: state of the salon's
    /// connection, members, and the latest engine / EasyTier errors.
    pub fn diagnostic(&self, id: &str) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let i = self.inner.lock().unwrap();
        let salon = i.salons.iter().find(|s| s.id == id);
        let _ = writeln!(out, "UberConnector {} — diagnostic du {}", env!("CARGO_PKG_VERSION"), now_text());
        let _ = writeln!(
            out,
            "Salon : {} ({}) · {}{}",
            salon.map_or("?", |s| s.name.as_str()),
            id.get(..8).unwrap_or(id),
            if i.settings.game_salon.as_deref() == Some(id) { "en jeu" } else { "chat seulement" },
            if salon.is_some_and(|s| s.gated) { " · validation des nouveaux" } else { "" },
        );
        let _ = writeln!(out, "Moi : {} ({})", i.settings.name, self.me.id);
        match i.statuses.get(id) {
            None => out.push_str("\nSalon non connecté.\n"),
            Some(st) => {
                let _ = writeln!(
                    out,
                    "Étape : {} · IP virtuelle : {} · adresse publique : {} · NAT : {} · port ouvert sur la box : {}",
                    st.stage,
                    st.my_vip.map(|v| v.to_string()).unwrap_or("–".into()),
                    st.public_punch.map(|a| a.to_string()).unwrap_or("–".into()),
                    if st.nat.is_empty() { "?" } else { st.nat.as_str() },
                    if st.port_open.is_empty() { "non" } else { st.port_open.as_str() }
                );
                out.push_str("\n== Chronologie\n");
                for t in &st.timings {
                    let _ = writeln!(out, "{}", t.trim_start());
                }
                out.push_str("\n== Joueurs\n");
                for m in &st.members {
                    let _ = writeln!(
                        out,
                        "{} · {} · {}{} · {} ms · perte {} · NAT {} · chat {}",
                        m.name,
                        m.vip,
                        m.path,
                        if m.relay_via.is_empty() { String::new() } else { format!(" par {}", m.relay_via) },
                        m.latency_ms,
                        m.loss,
                        m.nat,
                        if m.chat { "oui" } else { "non" }
                    );
                    if let Some(p) = &m.ping {
                        let _ = writeln!(
                            out,
                            "  ping mesuré (5 min, {} sondes) : min {} · médiane {} · moy {} · 95 % {} · max {} ms · gigue {:.1} ms · pics +{} ms : {} · perdues {}",
                            p.sent, p.min, p.median, p.avg, p.p95, p.max, p.jitter, uc_core::gametest::SPIKE_MS, p.spikes, p.lost
                        );
                        let _ = writeln!(out, "  dernière minute : {}", sparkline(&p.recent));
                    }
                }
                out.push_str("\n== Chemins (heure locale)\n");
                for l in &st.path_log {
                    let _ = writeln!(out, "{l}");
                }
                if let Some(t) = &st.game_test {
                    out.push_str("\n== Test du réseau de jeu\n");
                    if let Some(e) = &t.error {
                        let _ = writeln!(out, "impossible : {e}");
                    }
                    let yes = |b: bool| if b { "oui" } else { "non" };
                    for r in &t.results {
                        let _ = writeln!(
                            out,
                            "{} · direct {} · diffusion salon {} · annonce IP choisie {} · annonce automatique {}",
                            r.name,
                            r.direct_ms.map(|m| format!("{m} ms")).unwrap_or("non".into()),
                            yes(r.subnet),
                            yes(r.global_bound),
                            yes(r.global)
                        );
                    }
                    match t.relayed {
                        Some(n) => {
                            let _ = writeln!(out, "annonces relayées vers le salon : {n}");
                        }
                        None => out.push_str("relais des annonces : inactif
"),
                    }
                    if !t.rivals.is_empty() {
                        let _ = writeln!(out, "cartes concurrentes pour les annonces : {}", t.rivals.join(", "));
                    }
                }
                out.push_str("\n== Événements\n");
                for e in &st.events {
                    let _ = writeln!(out, "{e}");
                }
            }
        }
        drop(i);
        out.push_str("\n== Moteur (dernières lignes)\n");
        out.push_str(&tail(&self.data_dir.join("uc-helper.log"), 45, |_| true));
        out.push_str("\n== EasyTier (dernières erreurs)\n");
        out.push_str(&tail(&self.data_dir.join("logs").join("easytier-stderr.log"), 12, |l| {
            (l.contains("ERROR") || l.contains("panicked"))
                && !l.contains("bind addr fail")
                && !l.contains("lookup host for stun")
        }));
        out
    }

    /// Game mode: tests the salon's virtual LAN (results in the status).
    pub fn game_test(&self, id: &str) {
        self.inner.lock().unwrap().send(Command::GameTest { salon_id: id.to_string() });
    }

    pub fn chat(&self, id: &str) -> Vec<ChatLine> {
        self.inner.lock().unwrap().chats.get(id).cloned().unwrap_or_default()
    }

    /// Removes a salon from our list (members only; owners delete it).
    pub fn forget(&self, id: &str) -> Result<()> {
        let me = self.me.public_key_hex();
        if self.inner.lock().unwrap().salons.iter().any(|s| s.id == id && s.owner == me && !s.deleted) {
            bail!(tr!("vous êtes propriétaire de ce salon : supprimez-le plutôt", "you own this room: delete it instead"));
        }
        self.disconnect(id);
        self.update(|i| i.salons.retain(|s| s.id != id));
        self.persist();
        Ok(())
    }

    async fn owner_edit(&self, id: &str, busy: &str, edit: impl FnOnce(&mut Salon)) -> Result<()> {
        let _edit = self.edits.lock().await;
        let mut salon = self
            .inner
            .lock()
            .unwrap()
            .salons
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .context(tr!("salon inconnu", "unknown room"))?;
        edit(&mut salon);
        self.with_busy(busy, async { self.directory().await?.publish(&self.me, &salon).await })
            .await?;
        self.update(|i| {
            if let Some(s) = i.salons.iter_mut().find(|s| s.id == id) {
                *s = salon;
            }
            // Connected members refetch the manifest now instead of in 20 min.
            i.send(Command::NotifySalonChanged { salon_id: id.to_string() });
        });
        self.persist();
        Ok(())
    }

    pub async fn rename(&self, id: &str, name: &str) -> Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            bail!(tr!("le nom ne peut pas être vide", "the name cannot be empty"));
        }
        self.owner_edit(id, tr!("Renommage…", "Renaming…"), |s| s.name = name).await
    }

    pub async fn set_salon_avatar(&self, id: &str, avatar: &str) -> Result<()> {
        let avatar = avatar.trim().chars().take(72).collect::<String>();
        self.owner_edit(id, tr!("Mise à jour de l'avatar du salon…", "Updating the room avatar…"), |s| s.avatar = avatar).await
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.owner_edit(id, tr!("Suppression du salon…", "Deleting the room…"), |s| s.deleted = true).await?;
        self.disconnect(id);
        Ok(())
    }

    /// Refetches one salon's manifest right away (owner's change hint).
    /// Retried for a while: the DHT can lag behind the hint.
    async fn refresh_one(&self, id: &str) {
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
            if self.refresh_once(id).await {
                return;
            }
        }
    }

    /// One refetch of a salon; true if something changed (applied).
    async fn refresh_once(&self, id: &str) -> bool {
        let Ok(dir) = self.directory().await else { return false };
        let _edit = self.edits.lock().await;
        let Some(mut s) = self.inner.lock().unwrap().salons.iter().find(|s| s.id == id).cloned() else { return false };
        if let Ok(Refresh::Updated) = dir.refresh(&self.me, &mut s).await {
            self.apply_refreshed(s);
            return true;
        }
        false
    }

    /// A member coming back after a key change (ban, approval switched on)
    /// picks the new key up now, not at the next 20-minute refresh: the
    /// session restarts with it within seconds (see `apply_refreshed`).
    fn refresh_on_connect(self: &Arc<Self>, ids: Vec<String>) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            for id in ids {
                me.refresh_once(&id).await;
            }
        });
    }

    /// Applies a refreshed salon: new key -> reconnect, bans -> chat,
    /// deleted / banned / key lost -> disconnect.
    fn apply_refreshed(&self, s: Salon) {
        let (id, name) = (s.id.clone(), s.name.clone());
        let me_banned = s.banned.contains(&self.me.id);
        let lost = if s.deleted {
            Some(trf!("« {name} » a été supprimé par son propriétaire", "\"{name}\" was deleted by its owner"))
        } else if me_banned {
            Some(trf!("Vous avez été banni de « {name} »", "You were banned from \"{name}\""))
        } else if s.pending {
            Some(trf!("« {name} » demande maintenant une validation : vous êtes en salle d'attente", "\"{name}\" now requires approval: you are in the waiting room"))
        } else if s.key_lost {
            Some(trf!("« {name} » a changé de clé sans vous la transmettre : demandez le nouveau code d'invitation", "\"{name}\" changed its key without passing it to you: ask for the new invite code"))
        } else {
            None
        };
        let connected = self.update(|i| {
            let Some(x) = i.salons.iter_mut().find(|x| x.id == id) else { return false };
            let key_changed = x.access_key != s.access_key;
            let bans_changed = x.banned != s.banned;
            // Keep members met meanwhile (the refresh started from an older copy).
            let mut s = s;
            for m in std::mem::take(&mut x.roster) {
                s.remember(m);
            }
            *x = s;
            let connected = i.settings.connected.contains(&id);
            if connected && bans_changed {
                let banned = i.salons.iter().find(|x| x.id == id).unwrap().banned.clone();
                i.send(Command::SetBanned { salon_id: id.clone(), banned });
            }
            if connected && key_changed {
                i.rejoin(&id);
            }
            connected
        });
        if let (Some(msg), true) = (lost, connected) {
            self.disconnect(&id);
            self.update(|i| i.error = Some(msg));
        }
        self.persist();
    }

    // ---- Moderation (owner and moderators) ----

    /// Edits the salon, publishes our item (rotating the key if asked), then
    /// tells connected members to refetch, and reconnects with the new key.
    async fn moderate(
        &self,
        id: &str,
        busy: &str,
        rotate: bool,
        edit: impl FnOnce(&mut Salon, &Identity) -> Result<()>,
    ) -> Result<usize> {
        let _edit = self.edits.lock().await;
        let mut salon = self.salon(id)?;
        if !salon.can_moderate(&self.me) {
            bail!(tr!("seuls le propriétaire et les modérateurs peuvent faire ça", "only the owner and moderators can do that"));
        }
        edit(&mut salon, &self.me)?;
        let sent = self
            .with_busy(busy, async {
                let dir = self.directory().await?;
                if rotate {
                    return dir.rotate(&self.me, &mut salon).await;
                }
                if salon.is_owned_by(&self.me) {
                    dir.publish(&self.me, &salon).await?;
                } else {
                    dir.publish_mod(&self.me, &salon).await?;
                }
                Ok(0)
            })
            .await?;
        // Members refetch through the current (old) network before we leave it.
        self.update(|i| {
            i.send(Command::NotifySalonChanged { salon_id: id.to_string() });
            i.send(Command::SetBanned { salon_id: id.to_string(), banned: salon.banned.clone() });
        });
        if rotate {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        self.update(|i| {
            if let Some(x) = i.salons.iter_mut().find(|x| x.id == id) {
                // Keep members met meanwhile (our copy predates the DHT work),
                // except the ones just banned.
                for m in std::mem::take(&mut x.roster) {
                    if !salon.banned.contains(&m.id) {
                        salon.remember(m);
                    }
                }
                *x = salon;
            }
            if rotate {
                i.rejoin(id);
            }
        });
        self.persist();
        Ok(sent)
    }

    fn salon(&self, id: &str) -> Result<Salon> {
        self.inner.lock().unwrap().salons.iter().find(|s| s.id == id).cloned().context(tr!("salon inconnu", "unknown room"))
    }

    /// Bans a member: refused by the chat, then the key and invite change.
    pub async fn ban(&self, id: &str, member: &str) -> Result<usize> {
        self.moderate(id, tr!("Bannissement et changement de clé…", "Banning and changing the key…"), true, |s, me| {
            if member == me.id || s.owner.starts_with(member) {
                bail!(tr!("impossible de bannir ce membre", "this member cannot be banned"));
            }
            if s.is_owned_by(me) {
                if s.owner_banned.len() >= uc_core::salons::MAX_OWNER_BANS {
                    bail!(tr!("liste de bannis pleine", "the ban list is full"));
                }
                s.owner_banned.push(member.to_string());
                // A banned moderator loses the role.
                s.mods.retain(|pk| !pk.starts_with(member));
            } else {
                if s.mods.iter().any(|pk| pk.starts_with(member)) {
                    bail!(tr!("seul le propriétaire peut bannir un modérateur", "only the owner can ban a moderator"));
                }
                if s.my_banned.len() >= uc_core::salons::MAX_MOD_BANS {
                    bail!(tr!("liste de bannis pleine", "the ban list is full"));
                }
                s.my_banned.push(member.to_string());
            }
            s.banned.push(member.to_string());
            s.banned.sort();
            s.banned.dedup();
            Ok(())
        })
        .await
    }

    pub async fn unban(&self, id: &str, member: &str) -> Result<()> {
        self.moderate(id, tr!("Levée du bannissement…", "Lifting the ban…"), false, |s, me| {
            let listed = if s.is_owned_by(me) { &mut s.owner_banned } else { &mut s.my_banned };
            if !listed.iter().any(|b| b == member) {
                bail!(tr!("ce bannissement a été prononcé par quelqu'un d'autre", "this ban was issued by someone else"));
            }
            listed.retain(|b| b != member);
            s.banned.retain(|b| b != member);
            Ok(())
        })
        .await
        .map(|_| ())
    }

    /// New access key and invite code (e.g. after the code leaked).
    pub async fn regenerate(&self, id: &str) -> Result<usize> {
        self.moderate(id, tr!("Nouveau code et nouvelle clé…", "New code and new key…"), true, |_, _| Ok(())).await
    }

    /// Owner: grants or removes the moderator role.
    pub async fn set_moderator(&self, id: &str, member: &str, on: bool) -> Result<()> {
        let salon = self.salon(id)?;
        if !salon.is_owned_by(&self.me) {
            bail!(tr!("seul le propriétaire peut nommer des modérateurs", "only the owner can appoint moderators"));
        }
        let pk = salon
            .roster
            .iter()
            .find(|m| m.id == member)
            .map(|m| m.pk.clone())
            .context(tr!("membre inconnu : il doit avoir été connecté au chat en même temps que vous", "unknown member: they must have been in the chat at the same time as you"))?;
        self.moderate(id, tr!("Mise à jour des modérateurs…", "Updating moderators…"), false, |s, _| {
            s.mods.retain(|m| *m != pk);
            if on {
                if s.mods.len() >= uc_core::salons::MAX_MODS {
                    bail!(trf!("{} modérateurs maximum", "{} moderators maximum", uc_core::salons::MAX_MODS));
                }
                s.mods.push(pk);
            }
            Ok(())
        })
        .await
        .map(|_| ())
    }

    /// Owner: newcomers must be admitted (`on`), or join with the code alone.
    /// Turning it on changes the key: the code alone gave it until now.
    pub async fn set_gated(&self, id: &str, on: bool) -> Result<usize> {
        if !self.salon(id)?.is_owned_by(&self.me) {
            bail!(tr!("seul le propriétaire peut changer l'accès au salon", "only the owner can change access to the room"));
        }
        let busy = if on {
            tr!("Activation de la validation : nouvelle clé distribuée aux membres…", "Enabling approval: handing the new key to members…")
        } else {
            tr!("Ouverture du salon…", "Opening the room…")
        };
        self.moderate(id, busy, on, |s, _| {
            s.gated = on;
            Ok(())
        })
        .await
    }

    /// Owner or moderator: lets a waiting newcomer in.
    pub async fn admit(&self, id: &str, member: &str) -> Result<()> {
        let mut salon = self.salon(id)?;
        let knock = self
            .inner
            .lock()
            .unwrap()
            .knocks
            .get(id)
            .and_then(|l| l.iter().find(|k| k.id == member).cloned())
            .context(tr!("cette demande n'est plus en attente", "this request is no longer pending"))?;
        self.with_busy(&trf!("Validation de {}…", "Admitting {}…", knock.name), async {
            self.directory().await?.admit(&self.me, &mut salon, &knock).await
        })
        .await?;
        self.update(|i| {
            if let Some(x) = i.salons.iter_mut().find(|x| x.id == id) {
                x.remember(knock.member());
            }
            i.knocks_done.insert((id.to_string(), member.to_string()));
        });
        self.persist();
        Ok(())
    }

    /// Owner or moderator: hides a waiting newcomer (until the app restarts).
    pub fn dismiss(&self, id: &str, member: &str) {
        self.update(|i| {
            i.knocks_done.insert((id.to_string(), member.to_string()));
        });
    }

    /// Newcomer: checks for our admission, and knocks again when `knock`.
    async fn pending_round(self: &Arc<Self>, id: &str, knock: bool) {
        let Ok(dir) = self.directory().await else { return };
        let edit = self.edits.lock().await;
        let Ok(mut s) = self.salon(id) else { return };
        if !s.pending {
            return;
        }
        if knock {
            if let Ok(Refresh::Updated) = dir.refresh(&self.me, &mut s).await {
                if !s.pending || s.deleted {
                    self.apply_refreshed(s);
                    return;
                }
            }
            let (name, avatar) = {
                let i = self.inner.lock().unwrap();
                (i.settings.name.clone(), i.settings.avatar.clone())
            };
            // Failures are retried at the next knock round.
            let _ = dir.knock(&self.me, &s, &name, &avatar).await;
        }
        if let Ok(true) = dir.check_admission(&self.me, &mut s).await {
            let name = s.name.clone();
            self.update(|i| {
                if let Some(x) = i.salons.iter_mut().find(|x| x.id == id) {
                    *x = s;
                }
            });
            self.persist();
            // Connecting may wait for the UAC prompt: not under the lock.
            drop(edit);
            let _ = self.app.emit("notice", trf!("Vous avez été accepté dans « {name} »", "You were admitted to \"{name}\""));
            self.connect(id).await;
        }
    }

    /// Background duty: waiting rooms of gated salons (ours as newcomer, and
    /// those we moderate).
    pub async fn lobby_loop(self: Arc<Self>) {
        let mut round = 0u32;
        loop {
            let salons = self.inner.lock().unwrap().salons.clone();
            for s in salons.iter().filter(|s| s.pending && !s.deleted) {
                self.pending_round(&s.id, round % KNOCK_ROUNDS == 0).await;
            }
            let watched: Vec<&Salon> =
                salons.iter().filter(|s| s.gated && !s.pending && !s.deleted && s.can_moderate(&self.me)).collect();
            if !watched.is_empty() {
                if let Ok(dir) = self.directory().await {
                    for s in watched {
                        self.check_knocks(&dir, s).await;
                    }
                }
            }
            round += 1;
            tokio::time::sleep(LOBBY_EVERY).await;
        }
    }

    /// Owner or moderator: reads a salon's waiting room, announces newcomers.
    async fn check_knocks(&self, dir: &Directory, s: &Salon) {
        let knocks = dir.knocks(s).await;
        let ids = |l: &[Knock]| l.iter().map(|k| k.id.clone()).collect::<Vec<_>>();
        let changed = self.inner.lock().unwrap().knocks.get(&s.id).is_none_or(|old| ids(old) != ids(&knocks));
        if changed {
            // Announced once per newcomer: a failed DHT read (empty
            // list) must not make the next one look new.
            let fresh: Vec<&str> = {
                let mut i = self.inner.lock().unwrap();
                let i = &mut *i;
                knocks
                    .iter()
                    .filter(|k| {
                        let key = (s.id.clone(), k.id.clone());
                        !i.knocks_done.contains(&key) && i.knocks_announced.insert(key)
                    })
                    .map(|k| k.name.as_str())
                    .collect()
            };
            if !fresh.is_empty() {
                let _ = self.app.emit("notice", trf!("Quelqu'un attend une validation dans « {} »", "Someone is waiting for approval in \"{}\"", s.name));
                let title = trf!("Salle d'attente de « {} »", "Waiting room of \"{}\"", s.name);
                let body = trf!("{} attend votre validation", "{} is waiting for your approval", fresh.join(", "));
                self.notify(|c| c.notify_knock, &s.id, &title, &body);
            }
            self.update(|i| {
                i.knocks.insert(s.id.clone(), knocks);
            });
        }
    }

    /// Background duty: items pushed through Nostr (see `uc_core::push`):
    /// knocks, admissions and salon changes arrive in about a second
    /// instead of at the next DHT round. `UC_NO_NOSTR=1` turns it off.
    pub async fn push_loop(self: Arc<Self>) {
        if std::env::var_os("UC_NO_NOSTR").is_some() {
            return;
        }
        let dir = loop {
            match self.directory().await {
                Ok(d) => break d,
                Err(_) => tokio::time::sleep(Duration::from_secs(30)).await,
            }
        };
        let mut rx = dir.enable_push().subscribe();
        loop {
            let salons = self.inner.lock().unwrap().salons.clone();
            dir.watch(&salons);
            tokio::select! {
                _ = self.salons_changed.notified() => {}
                ev = rx.recv() => match ev {
                    Ok(p) => self.on_pushed(&dir, p, &salons),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    fn on_pushed(self: &Arc<Self>, dir: &Arc<Directory>, p: uc_core::push::Pushed, salons: &[Salon]) {
        use uc_core::push::What;
        let Some(s) = salons.iter().find(|s| s.id == p.salon_id).cloned() else { return };
        let me = self.clone();
        let dir = dir.clone();
        match p.what {
            What::Admission { member } if member == self.me.id && s.pending => {
                tauri::async_runtime::spawn(async move { me.pending_round(&s.id, false).await });
            }
            What::Knock if s.gated && !s.pending && s.can_moderate(&self.me) => {
                tauri::async_runtime::spawn(async move { me.check_knocks(&dir, &s).await });
            }
            What::Changed if !s.pending => {
                if !self.refreshing.lock().unwrap().insert(s.id.clone()) {
                    return;
                }
                tauri::async_runtime::spawn(async move {
                    me.refresh_one(&s.id).await;
                    me.refreshing.lock().unwrap().remove(&s.id);
                });
            }
            _ => {}
        }
    }

    // ---- Images (custom avatars) ----

    pub fn save_image(&self, bytes: &[u8]) -> Result<String> {
        uc_core::images::save(&self.data_dir, bytes)
    }

    /// Data URL of a cached image; `None` asks connected members for it.
    pub fn image(&self, hash: &str) -> Option<String> {
        use base64::Engine as _;
        let Some(bytes) = uc_core::images::load(&self.data_dir, hash) else {
            self.inner.lock().unwrap().send(Command::WantImage { hash: hash.to_string() });
            return None;
        };
        let mime = uc_core::images::mime(&bytes)?;
        Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
    }

    /// Background duty: keep salons alive in the DHT, apply owners' changes.
    pub async fn refresh_loop(self: Arc<Self>) {
        loop {
            if let Ok(dir) = self.directory().await {
                let ids: Vec<String> = self.inner.lock().unwrap().salons.iter().map(|s| s.id.clone()).collect();
                for id in ids {
                    // The salon as it is now, not as it was when the round began.
                    let _edit = self.edits.lock().await;
                    let Ok(mut s) = self.salon(&id) else { continue };
                    if let Ok(Refresh::Updated) = dir.refresh(&self.me, &mut s).await {
                        self.apply_refreshed(s);
                    }
                }
            }
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    }

    // ---- Settings & profile ----

    pub fn save(&self, mut new: Settings) -> Result<()> {
        new.name = new.name.trim().chars().take(24).collect();
        if new.name.is_empty() {
            bail!(tr!("le pseudo ne peut pas être vide", "the nickname cannot be empty"));
        }
        new.relays.retain(|r| !r.trim().is_empty());
        if new.custom_port != 0 && new.custom_port < 1024 {
            bail!(tr!("le port doit être vide ou compris entre 1024 et 65535", "the port must be empty or between 1024 and 65535"));
        }
        let autostart_changed = {
            let i = self.inner.lock().unwrap();
            // The switch and connections are driven elsewhere; keep them.
            new.enabled = i.settings.enabled;
            new.connected = i.settings.connected.clone();
            i.settings.autostart != new.autostart
        };
        if autostart_changed {
            settings::apply_autostart(new.autostart)?;
        }
        self.update(|i| {
            let profile_changed = i.settings.name != new.name || i.settings.avatar != new.avatar;
            crate::i18n::set(&new.lang);
            i.settings = new;
            // Applied live by every connected salon.
            if profile_changed {
                i.send(i.profile());
            }
        });
        self.persist();
        Ok(())
    }

    /// Asks the helper to stop; it also stops by itself if this process dies.
    pub fn shutdown(&self) {
        let mut i = self.inner.lock().unwrap();
        i.send(Command::Quit);
        i.tx = None;
    }

    // ---- Helper process ----

    async fn start_helper(self: &Arc<Self>, generation: u64) -> Result<mpsc::UnboundedSender<Command>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let token = hex::encode(rand::random::<[u8; 16]>());
        let exe = helper_path()?;
        let args = format!("helper --port {port} --token {token}");
        if uc_core::easytier::test_no_tun() {
            // Test mode: no adapter, so no elevation (no UAC prompt).
            std::process::Command::new(&exe).args(args.split(' ')).spawn()?;
        } else {
            tokio::task::spawn_blocking(move || elevate(&exe, &args)).await??;
        }

        let (stream, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
            .await
            .context(tr!("le moteur ne répond pas", "the engine is not responding"))??;
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let hello = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .context(tr!("le moteur ne répond pas", "the engine is not responding"))??
            .context(tr!("moteur déconnecté", "engine disconnected"))?;
        match serde_json::from_str::<Event>(&hello)? {
            Event::Hello { token: t } if t == token => {}
            _ => bail!(tr!("connexion inattendue sur le canal du moteur", "unexpected connection on the engine channel")),
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
        tauri::async_runtime::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let quit = matches!(cmd, Command::Quit);
                let mut line = serde_json::to_vec(&cmd).unwrap();
                line.push(b'\n');
                if w.write_all(&line).await.is_err() || quit {
                    break;
                }
            }
        });

        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(ev) = serde_json::from_str::<Event>(&line) else { continue };
                // Chat lines go out as their own event (no full state refresh).
                if let Event::ImageReady { hash } = &ev {
                    let _ = me.app.emit("image", hash);
                    continue;
                }
                if let Event::Member { salon_id, member } = &ev {
                    let (changed, first) = {
                        let mut i = me.inner.lock().unwrap();
                        let changed =
                            i.salons.iter_mut().find(|s| s.id == *salon_id).is_some_and(|s| s.remember(member.clone()));
                        let first = !i.seen.contains_key(&member.id);
                        if first {
                            i.seen.insert(member.id.clone(), now_secs());
                            save_seen(&me.data_dir, &i.seen);
                        }
                        (changed, first)
                    };
                    if changed {
                        me.persist();
                    }
                    if first {
                        me.changed();
                    }
                    continue;
                }
                if let Event::SalonChanged { salon_id } = &ev {
                    let (me2, id) = (me.clone(), salon_id.clone());
                    tauri::async_runtime::spawn(async move { me2.refresh_one(&id).await });
                    continue;
                }
                if let Event::Chat { salon_id, line } = ev {
                    let (keep, mention) = {
                        let mut i = me.inner.lock().unwrap();
                        let keep = i.generation == generation && i.settings.connected.contains(&salon_id);
                        let mention = (keep && !line.mine && !line.system && crate::notify::mentions(&line.text, &i.settings.name))
                            .then(|| i.salon_name(&salon_id));
                        if keep {
                            let log = i.chats.entry(salon_id.clone()).or_default();
                            log.push(line.clone());
                            let n = log.len();
                            if n > MAX_CHAT_LINES {
                                log.drain(..n - MAX_CHAT_LINES);
                            }
                        }
                        (keep, mention)
                    };
                    if let Some(salon) = mention {
                        let title = trf!("{} vous mentionne dans « {} »", "{} mentions you in \"{}\"", line.name, salon);
                        me.notify(|s| s.notify_mention, &salon_id, &title, &line.text);
                    }
                    if keep {
                        let _ = me.app.emit("chat", ChatEvent { salon_id, line });
                    }
                    continue;
                }
                let mut dirty = false;
                // (salon id, salon name, player name) of players who just came online.
                let mut arrivals = Vec::new();
                me.update(|i| {
                    if i.generation != generation {
                        return;
                    }
                    match ev {
                        // Ignore late statuses of salons we already left.
                        Event::Status(s) => {
                            if i.settings.connected.contains(&s.salon_id) {
                                let now = Instant::now();
                                if !i.statuses.contains_key(&s.salon_id) {
                                    i.status_since.insert(s.salon_id.clone(), now);
                                }
                                let settled = i.status_since.get(&s.salon_id).is_some_and(|t| t.elapsed() > ONLINE_QUIET);
                                let salon_name = i.salon_name(&s.salon_id);
                                let listed = i.last_listed.entry(s.salon_id.clone()).or_default();
                                for m in s.members.iter().filter(|m| !m.id.is_empty() && m.id != me.me.id && m.path != "vous") {
                                    let back = listed.get(&m.id).is_none_or(|t| t.elapsed() > ONLINE_GONE);
                                    if back && settled {
                                        arrivals.push((s.salon_id.clone(), salon_name.clone(), m.name.clone()));
                                    }
                                    listed.insert(m.id.clone(), now);
                                }
                                i.statuses.insert(s.salon_id.clone(), s);
                            }
                        }
                        Event::Left { salon_id } => {
                            i.statuses.remove(&salon_id);
                        }
                        // A failed session is disconnected (e.g. subnet already in use).
                        Event::Error { salon_id: Some(id), message } => {
                            i.error = Some(format!("{} : {message}", i.salon_name(&id)));
                            i.settings.connected.retain(|c| *c != id);
                            i.statuses.remove(&id);
                            i.chats.remove(&id);
                            if i.settings.game_salon.as_deref() == Some(id.as_str()) {
                                i.settings.game_salon = None;
                            }
                            dirty = true;
                        }
                        Event::Error { salon_id: None, message } => i.error = Some(message),
                        Event::Hello { .. }
                        | Event::Chat { .. }
                        | Event::ImageReady { .. }
                        | Event::Member { .. }
                        | Event::SalonChanged { .. } => {}
                    }
                });
                if dirty {
                    me.persist();
                }
                for (salon_id, salon, name) in arrivals {
                    me.notify(|s| s.notify_online, &salon_id, &trf!("{name} est en ligne", "{name} is online"), &trf!("dans « {salon} »", "in \"{salon}\""));
                }
            }
            me.update(|i| {
                if i.generation == generation && i.phase == Phase::On {
                    i.phase = Phase::Off;
                    i.settings.enabled = false;
                    i.tx = None;
                    i.statuses.clear();
                    i.chats.clear();
                    i.error = Some(tr!("Le moteur s'est arrêté (voir uc-data\\uc-helper.log)", "The engine stopped (see uc-data\\uc-helper.log)").into());
                }
            });
        });
        Ok(tx)
    }
}

/// The last `n` lines of a log file kept by `keep` (or a note if none).
fn tail(path: &Path, n: usize, keep: impl Fn(&str) -> bool) -> String {
    let Ok(bytes) = std::fs::read(path) else { return "(fichier absent)\n".into() };
    let text = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(256 * 1024)..]).into_owned();
    let lines: Vec<&str> = text.lines().filter(|l| keep(l)).collect();
    if lines.is_empty() {
        return "(rien)\n".into();
    }
    lines[lines.len().saturating_sub(n)..].iter().map(|l| format!("{l}\n")).collect()
}

/// "28/10/2026 19:05 UTC".
/// One block per ping sample ("×" when lost), scaled from 0 to the highest
/// value, which is written after it.
fn sparkline(samples: &[Option<u32>]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = samples.iter().flatten().copied().max().unwrap_or(0).max(1);
    let line: String = samples
        .iter()
        .map(|s| s.map_or('×', |v| BARS[(v as usize * (BARS.len() - 1) + max as usize / 2) / max as usize]))
        .collect();
    format!("{line} (max {max} ms)")
}

fn now_text() -> String {
    let secs = now_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{day:02}/{month:02}/{year} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Without the file yet, members already in our rosters count as long known.
fn load_seen(data_dir: &Path, salons: &[Salon]) -> HashMap<String, u64> {
    if let Some(seen) = std::fs::read_to_string(data_dir.join("seen.json")).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        return seen;
    }
    let seen = salons.iter().flat_map(|s| &s.roster).map(|m| (m.id.clone(), 0)).collect();
    save_seen(data_dir, &seen);
    seen
}

fn save_seen(data_dir: &Path, seen: &HashMap<String, u64>) {
    if let Ok(text) = serde_json::to_string(seen) {
        let _ = std::fs::write(data_dir.join("seen.json"), text);
    }
}

/// Anti-abuse hints for the members shown in our connected salons and the
/// newcomers waiting in our waiting rooms.
fn people(i: &Inner, my_id: &str) -> HashMap<String, PersonView> {
    let now = now_secs();
    let shown = i.statuses.values().flat_map(|st| st.members.iter().map(|m| &m.id));
    let waiting = i.knocks.values().flatten().map(|k| &k.id);
    shown
        .chain(waiting)
        .filter(|id| !id.is_empty() && *id != my_id)
        .map(|id| {
            let banned_in = i
                .salons
                .iter()
                .filter(|s| !s.deleted && s.banned.contains(id))
                .map(|s| SalonRef { id: s.id.clone(), name: s.name.clone() })
                .collect();
            let noob = i.seen.get(id).is_none_or(|t| now.saturating_sub(*t) < NOOB_SECS);
            (id.clone(), PersonView { banned_in, noob })
        })
        .collect()
}

fn helper_path() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let p = exe.with_file_name("uc-helper.exe");
    if !p.exists() {
        bail!(tr!("uc-helper.exe introuvable à côté de l'application", "uc-helper.exe not found next to the application"));
    }
    Ok(p)
}

/// Starts `exe` elevated (UAC prompt), hidden.
fn elevate(exe: &Path, args: &str) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }
    let verb = wide("runas".as_ref());
    let file = wide(exe.as_os_str());
    let params = wide(args.as_ref());
    let dir = wide(exe.parent().unwrap_or(Path::new(".")).as_os_str());
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.lpDirectory = dir.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            let err = GetLastError();
            if err == 1223 {
                bail!(tr!("Autorisation administrateur refusée", "Administrator permission denied"));
            }
            bail!(trf!("impossible de lancer le moteur (erreur Windows {err})", "cannot start the engine (Windows error {err})"));
        }
        if !info.hProcess.is_null() {
            CloseHandle(info.hProcess);
        }
    }
    Ok(())
}
