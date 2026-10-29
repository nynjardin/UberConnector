//! Content-addressed image cache (custom avatars of players and salons):
//! `uc-data/images/<sha256 hex>`. Images travel between members over the
//! chat links; the hash makes any member a trustworthy source.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Avatars are resized by the app (128 px WebP): this is plenty.
pub const MAX_IMAGE: usize = 48 * 1024;

fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("images")
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// MIME type from the magic bytes; only formats a browser displays safely.
pub fn mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Stores an image and returns its hash.
pub fn save(data_dir: &Path, bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_IMAGE {
        bail!("image trop lourde ({} Ko maximum)", MAX_IMAGE / 1024);
    }
    if mime(bytes).is_none() {
        bail!("format d'image non pris en charge (PNG, JPEG ou WebP)");
    }
    let h = hash(bytes);
    std::fs::create_dir_all(dir(data_dir))?;
    let path = dir(data_dir).join(&h);
    if !path.exists() {
        std::fs::write(path, bytes)?;
    }
    Ok(h)
}

/// Stores an image received from someone else, if it matches `expected`.
pub fn save_verified(data_dir: &Path, expected: &str, bytes: &[u8]) -> Result<()> {
    if hash(bytes) != expected {
        bail!("image corrompue");
    }
    save(data_dir, bytes).map(|_| ())
}

pub fn load(data_dir: &Path, hash: &str) -> Option<Vec<u8>> {
    if !valid_hash(hash) {
        return None;
    }
    std::fs::read(dir(data_dir).join(hash)).ok()
}

pub fn has(data_dir: &Path, hash: &str) -> bool {
    valid_hash(hash) && dir(data_dir).join(hash).exists()
}

/// Hash of an "img:<hash>" avatar.
pub fn avatar_hash(avatar: &str) -> Option<&str> {
    avatar.strip_prefix("img:").filter(|h| valid_hash(h))
}
