//! Small JSON files of `uc-data` (identity, salons, settings): written
//! atomically, and never silently replaced when they cannot be read.

use std::path::{Path, PathBuf};

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Writes through a temporary file, then renames it over `path`: a crash or
/// power cut mid-write leaves the previous file intact.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = with_suffix(path, ".tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Reads a JSON file; `None` when it is absent or unreadable. An unreadable
/// file is first copied to `<name>.corrupt`: the caller starts over from
/// defaults and will overwrite it, but its content can still be recovered.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            let bad = with_suffix(path, ".corrupt");
            let _ = std::fs::copy(path, &bad);
            tracing::error!("{} illisible ({e}) : copie gardée dans {}", path.display(), bad.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_and_corrupt_copy() {
        let dir = std::env::temp_dir().join(format!("uc-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        assert_eq!(read_json::<Vec<u32>>(&path), None);
        write_atomic(&path, b"[1,2]").unwrap();
        write_atomic(&path, b"[3]").unwrap();
        assert_eq!(read_json::<Vec<u32>>(&path), Some(vec![3]));
        assert!(!dir.join("a.json.tmp").exists());
        std::fs::write(&path, b"[3").unwrap();
        assert_eq!(read_json::<Vec<u32>>(&path), None);
        assert_eq!(std::fs::read(dir.join("a.json.corrupt")).unwrap(), b"[3");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
