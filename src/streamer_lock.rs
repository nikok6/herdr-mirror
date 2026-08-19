use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct StreamerIdentity<'a> {
    pub transport: &'a str,
    pub controller_scope: &'a str,
    pub target: &'a str,
    pub session: Option<&'a str>,
    pub pane: &'a str,
}

impl StreamerIdentity<'_> {
    pub fn key(&self) -> String {
        let digest = Sha256::digest(self.metadata().as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub fn metadata(&self) -> String {
        let mut out = String::from("herdr-mirror-streamer-v1");
        for (name, value) in [
            ("transport", self.transport),
            ("scope", self.controller_scope),
            ("target", self.target),
            ("session", self.session.unwrap_or("<default>")),
            ("pane", self.pane),
        ] {
            out.push('|');
            out.push_str(name);
            out.push(':');
            out.push_str(&value.len().to_string());
            out.push(':');
            out.push_str(value);
        }
        out
    }
}

pub struct StreamerLock {
    file: File,
}

impl StreamerLock {
    pub fn acquire(state_dir: &Path, identity: &StreamerIdentity<'_>) -> crate::util::Result<Self> {
        let dir = state_dir.join("streamer-pids");
        fs::create_dir_all(&dir)?;
        let path = lock_path(state_dir, &identity.key());
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(crate::util::err(format!(
                "streamer already running for {} ({})",
                identity.pane,
                path.display()
            )));
        }
        let metadata = identity.metadata();
        file.set_len(0)?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "{metadata}")?;
        file.flush()?;
        let recorded = fs::read_to_string(&path)?;
        if recorded.lines().nth(1) != Some(metadata.as_str()) {
            return Err(crate::util::err(format!(
                "streamer lock metadata verification failed ({})",
                path.display()
            )));
        }
        Ok(Self { file })
    }
}

impl Drop for StreamerLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn lock_path(state_dir: &Path, key: &str) -> PathBuf {
    state_dir
        .join("streamer-pids")
        .join(format!("v1-{key}.lock"))
}

pub fn is_held(state_dir: &Path, key: &str) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path(state_dir, key))
    else {
        return false;
    };
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    }
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity<'a>(
        scope: &'a str,
        target: &'a str,
        session: Option<&'a str>,
    ) -> StreamerIdentity<'a> {
        StreamerIdentity {
            transport: "ssh",
            controller_scope: scope,
            target,
            session,
            pane: "w1:p1",
        }
    }

    #[test]
    fn key_distinguishes_scope_target_session_and_pane_without_lossy_sanitizing() {
        let base = identity("work", "user@host:a", None);
        let keys = [
            base.key(),
            identity("other", "user@host:a", None).key(),
            identity("work", "user@host/a", None).key(),
            identity("work", "user@host:a", Some("named")).key(),
            StreamerIdentity {
                pane: "w1:p2",
                ..base
            }
            .key(),
        ];
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
        assert!(keys.iter().all(|key| key.len() == 64));
    }

    #[test]
    fn held_lock_refuses_duplicate_and_drop_releases_it() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mirror-streamer-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let id = identity("work", "work", None);
        let first = StreamerLock::acquire(&dir, &id).unwrap();
        assert!(is_held(&dir, &id.key()));
        assert!(StreamerLock::acquire(&dir, &id).is_err());
        drop(first);
        assert!(!is_held(&dir, &id.key()));
        assert!(StreamerLock::acquire(&dir, &id).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
