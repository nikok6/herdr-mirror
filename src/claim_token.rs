use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::util::{err, Result};

const MAX_TOKEN_BYTES: u64 = 32;

fn path(state_dir: &Path, streamer_key: &str) -> PathBuf {
    state_dir
        .join("claim-tokens")
        .join(format!("v1-{streamer_key}.token"))
}

fn generation_path(state_dir: &Path, streamer_key: &str) -> PathBuf {
    state_dir
        .join("claim-tokens")
        .join(format!("v1-{streamer_key}.generation"))
}

pub(crate) fn load(state_dir: &Path, streamer_key: &str) -> Result<Option<u64>> {
    let path = path(state_dir, streamer_key);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > MAX_TOKEN_BYTES {
        return Err(err("terminal claim token file is oversized"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        return Err(err("terminal claim token file is oversized"));
    }
    let text = std::str::from_utf8(&bytes)?.trim();
    let token = text
        .parse::<u64>()
        .map_err(|_| err("terminal claim token file is invalid"))?;
    Ok(Some(token))
}

pub(crate) fn save(state_dir: &Path, streamer_key: &str, token: u64) -> Result<()> {
    save_number(&path(state_dir, streamer_key), token)
}

pub(crate) fn next_generation(state_dir: &Path, streamer_key: &str) -> Result<u64> {
    let path = generation_path(state_dir, streamer_key);
    let current = match read_number(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    let next = current
        .checked_add(1)
        .ok_or_else(|| err("terminal controller generation exhausted"))?;
    save_number(&path, next)?;
    Ok(next)
}

fn read_number(path: &Path) -> std::io::Result<u64> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_TOKEN_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal controller number file is oversized",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal controller number file is oversized",
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
        .trim();
    text.parse::<u64>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn save_number(path: &Path, value: u64) -> Result<()> {
    let dir = path.parent().ok_or_else(|| err("number path has no parent"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".number.{}-{}-{}.tmp",
        std::process::id(),
        value,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    writeln!(file, "{value}")?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrips_and_replaces_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mirror-claim-token-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir, "pane").unwrap(), None);
        save(&dir, "pane", 7).unwrap();
        assert_eq!(load(&dir, "pane").unwrap(), Some(7));
        save(&dir, "pane", 9).unwrap();
        assert_eq!(load(&dir, "pane").unwrap(), Some(9));
        assert_eq!(next_generation(&dir, "pane").unwrap(), 1);
        assert_eq!(next_generation(&dir, "pane").unwrap(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
