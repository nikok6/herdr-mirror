use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Once;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::{err, Result};

const ID_FILE: &str = "controller-id.json";
const MAX_IDENTITY_BYTES: u64 = 1024;
static MISSING_MACHINE_WARNING: Once = Once::new();

#[derive(Deserialize, Serialize)]
struct StoredIdentity {
    id: String,
    machine: Option<String>,
}

pub fn controller_id(state_dir: &Path, scope: &str) -> Result<String> {
    let stored = load_or_create(state_dir, machine_fingerprint())?;
    let mut hash = Sha256::new();
    hash.update(b"herdr-mirror-controller-v1");
    hash.update((stored.id.len() as u64).to_be_bytes());
    hash.update(stored.id.as_bytes());
    hash.update((scope.len() as u64).to_be_bytes());
    hash.update(scope.as_bytes());
    Ok(hex(&hash.finalize()))
}

fn load_or_create(state_dir: &Path, machine: Option<String>) -> Result<StoredIdentity> {
    fs::create_dir_all(state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(state_dir.join("controller-id.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let path = state_dir.join(ID_FILE);
    match read_identity_file(&path) {
        Ok(text) => {
            let mut stored = parse(&text)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            if stored.machine.is_some() && machine.is_some() && stored.machine != machine {
                let replacement = StoredIdentity {
                    id: random_id()?,
                    machine,
                };
                write_atomic(&path, &replacement)?;
                return Ok(replacement);
            }
            if stored.machine.is_none() && machine.is_some() {
                stored.machine = machine;
                write_atomic(&path, &stored)?;
            } else if stored.machine.is_none() {
                warn_missing_machine_fingerprint();
            }
            Ok(stored)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let created = StoredIdentity {
                id: random_id()?,
                machine,
            };
            if created.machine.is_none() {
                warn_missing_machine_fingerprint();
            }
            write_atomic(&path, &created)?;
            Ok(created)
        }
        Err(error) => Err(error.into()),
    }
}

fn read_identity_file(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_IDENTITY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "controller identity file is oversized",
        ));
    }
    let mut bytes = Vec::with_capacity(MAX_IDENTITY_BYTES as usize);
    file.take(MAX_IDENTITY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IDENTITY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "controller identity file is oversized",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "controller identity file is not UTF-8",
        )
    })
}

fn warn_missing_machine_fingerprint() {
    MISSING_MACHINE_WARNING.call_once(|| {
        eprintln!(
            "herdr-mirror: machine fingerprint unavailable; controller identity copied to another machine may collide"
        );
    });
}

fn parse(text: &str) -> Result<StoredIdentity> {
    if text.len() > 1024 {
        return Err(err("controller identity file is oversized"));
    }
    let stored: StoredIdentity = serde_json::from_str(text)?;
    let valid = stored.id.len() == 64 && stored.id.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid {
        return Err(err("controller identity file contains an invalid id"));
    }
    Ok(stored)
}

fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

fn write_atomic(path: &Path, identity: &StoredIdentity) -> Result<()> {
    let suffix = random_id()?;
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), &suffix[..12]));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(identity)?)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn machine_fingerprint() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(value) = fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.split_once("IOPlatformUUID")?.1.split('"').nth(2))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-mirror-controller-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn identity_is_stable_per_scope_and_distinct_across_host_entries() {
        let dir = temp_dir("scopes");
        let first = controller_id(&dir, "work").unwrap();
        assert_eq!(controller_id(&dir, "work").unwrap(), first);
        assert_ne!(controller_id(&dir, "alias-to-work").unwrap(), first);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copied_state_regenerates_on_machine_mismatch() {
        let dir = temp_dir("machine");
        let first = load_or_create(&dir, Some("machine-a".into())).unwrap();
        let second = load_or_create(&dir, Some("machine-b".into())).unwrap();
        assert_ne!(first.id, second.id);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_later_machine_fingerprint_binds_without_changing_the_identity() {
        let dir = temp_dir("late-machine");
        let first = load_or_create(&dir, None).unwrap();
        let bound = load_or_create(&dir, Some("machine-a".into())).unwrap();
        assert_eq!(first.id, bound.id);
        assert_eq!(bound.machine.as_deref(), Some("machine-a"));
        let replacement = load_or_create(&dir, Some("machine-b".into())).unwrap();
        assert_ne!(bound.id, replacement.id);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_creation_publishes_one_complete_identity() {
        let dir = temp_dir("concurrent");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    load_or_create(&dir, Some("machine".into())).unwrap().id
                })
            })
            .collect();
        let ids: std::collections::HashSet<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_and_oversized_identity_files_fail_clearly() {
        for (name, contents, expected) in [
            ("malformed", "not-json".to_owned(), "expected ident"),
            ("oversized", "x".repeat(1025), "oversized"),
        ] {
            let dir = temp_dir(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(ID_FILE), contents).unwrap();
            let error = load_or_create(&dir, Some("machine".into()))
                .err()
                .expect("invalid identity must fail")
                .to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
