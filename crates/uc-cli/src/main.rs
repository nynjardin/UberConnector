use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;
use uc_core::identity::Identity;
use uc_core::salons::{self, Directory, Salon};
use uc_core::session::{Profile, Session, SessionConfig, Status};

mod helper;

#[derive(Parser)]
#[command(name = "uc-helper", about = "LAN virtuel P2P pour jouer en réseau local à distance")]
struct Cli {
    /// Sans commande : mode interactif (double-clic)
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Crée un nouveau salon et s'y connecte
    New {
        /// Nom du salon
        #[arg(long = "salon", default_value = "Mon salon")]
        salon: String,
        #[command(flatten)]
        opts: JoinOpts,
    },
    /// Rejoint un salon avec son code d'invitation
    Join {
        /// Code d'invitation, ex. K7QM-ZXRA-PQ4H
        code: String,
        #[command(flatten)]
        opts: JoinOpts,
    },
    /// Moteur piloté par l'application UberConnector (usage interne)
    #[command(hide = true)]
    Helper {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        token: String,
    },
}

enum Target {
    New(String),
    Join(String),
}

fn init(data_dir: &std::path::Path) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)?;
    let log = std::fs::File::create(data_dir.join("uc-helper.log"))?;
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log))
        .with_ansi(false)
        .with_env_filter("info,mainline=error")
        .init();
    Ok(data_dir.to_path_buf())
}

#[derive(clap::Args)]
struct JoinOpts {
    /// Pseudo affiché aux autres joueurs
    #[arg(long)]
    name: Option<String>,
    /// Nœud EasyTier de secours (rendez-vous/relais), ex. tcp://mon-vps:11010
    #[arg(long = "relay")]
    relays: Vec<String>,
    /// Force l'IP virtuelle
    #[arg(long)]
    ip: Option<Ipv4Addr>,
    /// Interdit les raccourcis LAN (simule des joueurs distants sur un même réseau)
    #[arg(long)]
    no_lan: bool,
}

fn ask(q: &str) -> String {
    use std::io::Write;
    print!("{q}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().to_string()
}

fn interactive() -> (Target, JoinOpts) {
    println!("=== UberConnector ===\n");
    println!("  1) Créer un salon");
    println!("  2) Rejoindre un salon\n");
    let target = loop {
        match ask("Choix : ").as_str() {
            "1" => {
                let n = ask("Nom du salon : ");
                break Target::New(if n.is_empty() { "Mon salon".into() } else { n });
            }
            "2" => {
                let c = ask("Code d'invitation : ");
                if uc_core::room::InviteKeys::from_code(&c).is_ok() {
                    break Target::Join(c);
                }
                println!("Code invalide.");
            }
            _ => {}
        }
    };
    let name = ask("Pseudo (Entrée = garder le précédent) : ");
    let no_lan = ask("Simuler un joueur distant, test sur un même réseau local ? [o/N] : ")
        .to_lowercase()
        .starts_with('o');
    let opts = JoinOpts {
        name: (!name.is_empty()).then_some(name),
        relays: vec![],
        ip: None,
        no_lan,
    };
    (target, opts)
}

fn pause() {
    ask("\nAppuyez sur Entrée pour fermer…");
}

fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("UC_DATA_DIR") {
        return d.into();
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("uc-data")))
        .unwrap_or_else(|| "uc-data".into())
}

/// Creates (and publishes) or resolves the salon, and remembers it.
async fn open_salon(target: Target, data_dir: &std::path::Path) -> Result<Salon> {
    println!("Connexion à la DHT…");
    let dir = Directory::start().await?;
    let me = Identity::load_or_create(data_dir)?;
    let salon = match target {
        Target::New(name) => {
            let s = salons::create(&me, &name);
            dir.publish(&me, &s).await?;
            s
        }
        Target::Join(code) => {
            println!("Recherche de l'invitation…");
            dir.resolve(&me, &code).await?
        }
    };
    let mut all = salons::load(data_dir);
    all.retain(|s| s.id != salon.id);
    all.insert(0, salon.clone());
    salons::save(data_dir, &all)?;
    Ok(salon)
}

fn render(salon: &Salon, s: &Status) {
    let mut out = String::from("\x1b[2J\x1b[H");
    out += "=== UberConnector ===  Ctrl+C pour quitter\n\n";
    out += &format!("Salon      : {}   invitation : {}\n", salon.name, salon.invite);
    out += &format!(
        "Vous       : {}  IP virtuelle {}\n",
        s.my_name,
        s.my_vip.map(|i| i.to_string()).unwrap_or("…".into())
    );
    out += &format!(
        "NAT public : {}   slot DHT : {}\n\n",
        s.public_punch.map(|a| a.to_string()).unwrap_or("?".into()),
        s.dht_slot.map(|i| i.to_string()).unwrap_or("…".into())
    );
    out += &format!("{:<18} {:<14} {:>9} {:>7}  {:<30} {}\n", "JOUEUR", "IP", "PING ms", "PERTE", "LIAISON", "NAT");
    for m in &s.members {
        out += &format!(
            "{:<18} {:<14} {:>9} {:>7}  {:<30} {}\n",
            m.name, m.vip, m.latency_ms, m.loss, m.path, m.nat
        );
    }
    out += "\nÉvénements :\n";
    for e in &s.events {
        out += &format!("  {e}\n");
    }
    print!("{out}");
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_interactive = cli.cmd.is_none();
    if !uc_core::system::is_admin() && !uc_core::easytier::test_no_tun() {
        eprintln!("Droits administrateur requis (création de la carte réseau virtuelle).");
        eprintln!("Relancez en tant qu'administrateur.");
        if is_interactive {
            pause();
        }
        std::process::exit(1);
    }
    let (target, opts) = match cli.cmd {
        Some(Cmd::New { salon, opts }) => (Target::New(salon), opts),
        Some(Cmd::Join { code, opts }) => (Target::Join(code), opts),
        Some(Cmd::Helper { port, token }) => {
            let data_dir = init(&data_dir())?;
            return helper::run(port, token, data_dir).await;
        }
        None => interactive(),
    };

    let data_dir = init(&data_dir())?;
    let salon = match open_salon(target, &data_dir).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Erreur : {e:#}");
            if is_interactive {
                pause();
            }
            std::process::exit(1);
        }
    };
    // Kept alive for the whole session (the session watches it).
    let (_profile_tx, profile) = tokio::sync::watch::channel(Profile {
        name: opts.name.unwrap_or_default(),
        avatar: String::new(),
    });
    // The CLI has no chat UI; lines are dropped.
    let (events_out, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = Session::start(SessionConfig {
        salon_id: salon.id.clone(),
        access_key: salon.access_key_bytes(),
        profile,
        game: true,
        banned: salon.banned.clone(),
        events_out,
        relays: opts.relays,
        subnet: salon.subnet(),
        fixed_ip: opts.ip,
        use_lan: !opts.no_lan,
        port_map: true,
        custom_port: 0,
        data_dir,
    });
    let mut status = session.status.clone();
    let mut redraw = tokio::time::interval(Duration::from_millis(1000));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = redraw.tick() => {
                render(&salon, &status.borrow_and_update());
                if session.is_finished() {
                    break;
                }
            }
        }
    }
    let last = status.borrow().clone();
    let r = session.stop().await;
    if let Ok(dir) = uc_core::easytier::find_dir() {
        uc_core::system::unload_our_windivert(&dir);
    }
    render(&salon, &last);
    if is_interactive {
        if let Err(e) = &r {
            eprintln!("\nErreur : {e:#}");
        }
        pause();
    }
    r
}
