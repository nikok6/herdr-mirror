use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    legacy_path: PathBuf,
}

impl StreamerLock {
    pub fn acquire(state_dir: &Path, identity: &StreamerIdentity<'_>) -> crate::util::Result<Self> {
        let dir = state_dir.join("streamer-pids");
        fs::create_dir_all(&dir)?;
        let path = lock_path(state_dir, &identity.key());
        let directory_lock = coordination_lock(&dir, libc::LOCK_EX)?;
        collect_stale_files(&dir, &path)?;
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
        let mut existing = String::new();
        file.read_to_string(&mut existing)?;
        if let Some(recorded) = existing.lines().nth(1) {
            if recorded != metadata {
                return Err(crate::util::err(format!(
                    "streamer lock identity collision ({})",
                    path.display()
                )));
            }
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "{metadata}")?;
        file.flush()?;
        let legacy_path = legacy_pid_path(state_dir, identity.target, identity.pane);
        fs::write(&legacy_path, std::process::id().to_string())?;
        drop(directory_lock);
        Ok(Self { file, legacy_path })
    }
}

impl Drop for StreamerLock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.legacy_path)
            .ok()
            .is_some_and(|pid| pid.trim() == std::process::id().to_string())
        {
            let _ = fs::remove_file(&self.legacy_path);
        }
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn coordination_lock(dir: &Path, mode: libc::c_int) -> crate::util::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join(".directory.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), mode) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(file)
}

fn collect_stale_files(dir: &Path, retained_path: &Path) -> crate::util::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("v1-") && name.ends_with(".lock") {
            if path == retained_path {
                continue;
            }
            let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0
                && fs::remove_file(&path).is_ok()
            {
                if let Some(key) = name
                    .strip_prefix("v1-")
                    .and_then(|name| name.strip_suffix(".lock"))
                {
                    if let Some(state_dir) = dir.parent() {
                        let _ = fs::remove_file(
                            state_dir
                                .join("claim-tokens")
                                .join(format!("v1-{key}.token")),
                        );
                        let _ = fs::remove_file(
                            state_dir
                                .join("claim-tokens")
                                .join(format!("v1-{key}.generation")),
                        );
                    }
                }
            }
        } else if name.ends_with(".pid") {
            let alive = fs::read_to_string(&path)
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
                .is_some_and(crate::util::pid_alive);
            if !alive {
                let _ = fs::remove_file(path);
            }
        }
    }
    Ok(())
}

fn legacy_pid_path(state_dir: &Path, target: &str, pane: &str) -> PathBuf {
    let sanitized = |value: &str| {
        value
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect::<String>()
    };
    state_dir.join("streamer-pids").join(format!(
        "{}--{}.pid",
        sanitized(target),
        sanitized(pane)
    ))
}

pub fn lock_path(state_dir: &Path, key: &str) -> PathBuf {
    state_dir
        .join("streamer-pids")
        .join(format!("v1-{key}.lock"))
}

pub fn is_held(state_dir: &Path, key: &str) -> bool {
    let dir = state_dir.join("streamer-pids");
    let Ok(_directory_lock) = coordination_lock(&dir, libc::LOCK_SH) else {
        return false;
    };
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path(state_dir, key))
    else {
        return false;
    };
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
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

    #[test]
    fn acquire_collects_unlocked_artifacts_but_keeps_active_locks() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mirror-streamer-gc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let active_id = identity("active", "host", None);
        let active = StreamerLock::acquire(&dir, &active_id).unwrap();
        let stale_lock = lock_path(&dir, &"a".repeat(64));
        fs::write(&stale_lock, "stale").unwrap();
        let stale_pid = dir.join("streamer-pids/dead--pane.pid");
        fs::write(&stale_pid, "999999999").unwrap();

        let other = StreamerLock::acquire(&dir, &identity("other", "host", None)).unwrap();
        assert!(!stale_lock.exists());
        assert!(!stale_pid.exists());
        assert!(is_held(&dir, &active_id.key()));

        drop(other);
        drop(active);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_lock_metadata_is_validated_before_replacement() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mirror-streamer-metadata-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let id = identity("work", "host", None);
        let path = lock_path(&dir, &id.key());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "pid=1\nwrong identity\n").unwrap();
        let error = StreamerLock::acquire(&dir, &id)
            .err()
            .expect("metadata collision must fail")
            .to_string();
        assert!(error.contains("identity collision"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }
}
