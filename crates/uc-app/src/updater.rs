//! Updates from the GitHub releases: the latest release's zip is
//! downloaded (SHA-256 checked when GitHub gives it), unpacked next to the
//! app, and swapped in: files in use are renamed to `*.old` (Windows allows
//! renaming a running exe), the new ones take their place, then the new
//! version is started. `uc-data` (identity, salons, settings) is never
//! touched. Any failure puts the previous files back.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LATEST: &str = "https://api.github.com/repos/nynjardin/UberConnector/releases/latest";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The zip is expected well under this (a corrupted answer must not fill the disk).
const MAX_ZIP: u64 = 300 << 20;

#[derive(Serialize, Clone, Debug)]
pub struct Update {
    pub version: String,
    /// Release notes (Markdown, shown as text).
    pub notes: String,
    /// Release page on GitHub.
    pub page: String,
    url: String,
    size: u64,
    /// "sha256:<hex>" when GitHub computed it.
    digest: Option<String>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .build()
        .into()
}

/// "1.2.3" (or "v1.2.3", "1.2.3-beta") -> (1, 2, 3).
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut it = core.split('.').map(|n| n.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse(candidate), parse(current)), (Some(a), Some(b)) if a > b)
}

/// The latest release, if newer than us. Blocking (run it off the UI thread).
/// `UC_UPDATE_API` replaces GitHub's address (tests).
pub fn check() -> Result<Option<Update>> {
    let api = std::env::var("UC_UPDATE_API").unwrap_or_else(|_| LATEST.into());
    let mut resp = agent()
        .get(&api)
        .header("User-Agent", &format!("UberConnector/{CURRENT}"))
        .header("Accept", "application/vnd.github+json")
        .call();
    // No release published yet: nothing to offer.
    if let Err(ureq::Error::StatusCode(404)) = resp {
        return Ok(None);
    }
    let rel: Release = resp.as_mut().map_err(|e| anyhow::anyhow!("{e}"))?.body_mut().read_json().context("réponse de GitHub illisible")?;
    if rel.draft || rel.prerelease || !is_newer(&rel.tag_name, CURRENT) {
        return Ok(None);
    }
    let version = rel.tag_name.trim_start_matches(['v', 'V']).to_string();
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(&format!("UberConnector-{version}.zip")))
        .or_else(|| rel.assets.iter().find(|a| a.name.starts_with("UberConnector") && a.name.ends_with(".zip")))
        .with_context(|| format!("la version {version} n'a pas de zip à télécharger"))?;
    Ok(Some(Update {
        version,
        notes: rel.body.unwrap_or_default(),
        page: rel.html_url,
        url: asset.browser_download_url.clone(),
        size: asset.size,
        digest: asset.digest.clone(),
    }))
}

/// Downloads, checks and installs `u` into the app folder; returns the new
/// exe to start. The engine must be stopped first (its files get replaced).
/// `progress(done, total)` is called during the download.
pub fn install(u: &Update, work: &Path, progress: impl Fn(u64, u64)) -> Result<PathBuf> {
    let app_dir = std::env::current_exe()?.parent().context("dossier de l'application")?.to_path_buf();
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work)?;
    let zip_path = work.join("update.zip");
    download(u, &zip_path, progress)?;
    let staged = work.join("new");
    let files = unpack(&zip_path, &staged)?;
    if !files.iter().any(|f| f == Path::new("UberConnector.exe")) {
        bail!("le zip ne contient pas UberConnector.exe");
    }
    swap(&app_dir, &staged, &files)?;
    let _ = std::fs::remove_dir_all(work);
    Ok(app_dir.join("UberConnector.exe"))
}

fn download(u: &Update, to: &Path, progress: impl Fn(u64, u64)) -> Result<()> {
    if u.size > MAX_ZIP {
        bail!("mise à jour anormalement grosse ({} Mo)", u.size >> 20);
    }
    let mut resp = agent()
        .get(&u.url)
        .header("User-Agent", &format!("UberConnector/{CURRENT}"))
        .call()
        .map_err(|e| anyhow::anyhow!("téléchargement : {e}"))?;
    let mut reader = resp.body_mut().as_reader();
    let mut file = std::fs::File::create(to)?;
    let mut hash = Sha256::new();
    let (mut done, mut buf) = (0u64, vec![0u8; 64 * 1024]);
    loop {
        let n = reader.read(&mut buf).context("téléchargement interrompu")?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > MAX_ZIP {
            bail!("téléchargement anormalement gros");
        }
        hash.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        progress(done, u.size);
    }
    if u.size != 0 && done != u.size {
        bail!("téléchargement incomplet ({done} octets sur {})", u.size);
    }
    if let Some(want) = u.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
        let got = hex::encode(hash.finalize());
        if !got.eq_ignore_ascii_case(want) {
            bail!("le fichier téléchargé ne correspond pas à celui publié (empreinte SHA-256 différente)");
        }
    }
    Ok(())
}

/// Unpacks into `to`, without the zip's top folder ("UberConnector/") and
/// never `uc-data`. Returns the relative paths of the files.
fn unpack(zip_path: &Path, to: &Path) -> Result<Vec<PathBuf>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path)?).context("zip de mise à jour illisible")?;
    let mut files = vec![];
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        // `enclosed_name`: refuses "../" and absolute paths.
        let Some(path) = entry.enclosed_name() else { continue };
        let mut parts = path.components();
        if path.components().count() > 1 && path.starts_with("UberConnector") {
            parts.next();
        }
        let rel: PathBuf = parts.collect();
        if rel.as_os_str().is_empty() || rel.starts_with("uc-data") || entry.is_dir() {
            continue;
        }
        let out = to.join(&rel);
        std::fs::create_dir_all(out.parent().unwrap())?;
        std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
        files.push(rel);
    }
    Ok(files)
}

fn old_name(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

/// Puts the staged files in place; on any error, restores the previous ones.
fn swap(app_dir: &Path, staged: &Path, files: &[PathBuf]) -> Result<()> {
    // (installed file, its previous version renamed to .old, if any)
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = vec![];
    let r = (|| -> Result<()> {
        for rel in files {
            let target = app_dir.join(rel);
            std::fs::create_dir_all(target.parent().unwrap())?;
            let old = if target.exists() {
                let old = old_name(&target);
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&target, &old).with_context(|| format!("{} est verrouillé", rel.display()))?;
                Some(old)
            } else {
                None
            };
            done.push((target.clone(), old));
            std::fs::copy(staged.join(rel), &target).with_context(|| format!("copie de {}", rel.display()))?;
        }
        Ok(())
    })();
    if r.is_err() {
        for (target, old) in done.into_iter().rev() {
            let _ = std::fs::remove_file(&target);
            if let Some(old) = old {
                let _ = std::fs::rename(&old, &target);
            }
        }
    }
    r
}

/// Removes the `*.old` files left by the last update (once it has exited).
pub fn cleanup(app_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(app_dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.file_name().is_some_and(|n| n == "uc-data") {
            continue;
        }
        if p.is_dir() {
            cleanup(&p);
        } else if p.extension().is_some_and(|x| x == "old") {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("n'importe quoi", "0.1.0"));
    }

    #[test]
    fn unpack_swap_and_rollback() {
        let base = std::env::temp_dir().join(format!("uc-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (app, staged) = (base.join("app"), base.join("staged"));
        std::fs::create_dir_all(app.join("easytier")).unwrap();
        std::fs::create_dir_all(app.join("uc-data")).unwrap();
        std::fs::write(app.join("UberConnector.exe"), "v1").unwrap();
        std::fs::write(app.join("uc-data").join("identity.json"), "secret").unwrap();
        // A zip as package.ps1 makes it (top folder), with a uc-data that must be ignored.
        let zip_path = base.join("u.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            ("UberConnector/UberConnector.exe", "v2"),
            ("UberConnector/easytier/easytier-core.exe", "et2"),
            ("UberConnector/uc-data/identity.json", "écrasé ?"),
            ("../evil.txt", "x"),
        ] {
            z.start_file(name, opts).unwrap();
            z.write_all(data.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        let files = unpack(&zip_path, &staged).unwrap();
        assert_eq!(files.len(), 2, "{files:?}");
        swap(&app, &staged, &files).unwrap();
        assert_eq!(std::fs::read_to_string(app.join("UberConnector.exe")).unwrap(), "v2");
        assert_eq!(std::fs::read_to_string(app.join("easytier/easytier-core.exe")).unwrap(), "et2");
        assert_eq!(std::fs::read_to_string(app.join("uc-data/identity.json")).unwrap(), "secret");
        assert!(!base.join("evil.txt").exists());
        cleanup(&app);
        assert!(!app.join("UberConnector.exe.old").exists());
        // A file that cannot be copied: everything is put back.
        std::fs::write(app.join("UberConnector.exe"), "v2").unwrap();
        let files = vec![PathBuf::from("UberConnector.exe"), PathBuf::from("absent.dll")];
        assert!(swap(&app, &staged, &files).is_err());
        assert_eq!(std::fs::read_to_string(app.join("UberConnector.exe")).unwrap(), "v2");
        assert!(!app.join("absent.dll").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
