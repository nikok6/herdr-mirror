//! Windows server-mode relay.
//!
//! This mode deliberately has a very small trust boundary: the only child it
//! starts is the `herdr.exe` next to this executable, and the only endpoint it
//! accepts is the socket reported by that child. Once connected, stdout is
//! reserved for the opaque pipe protocol; diagnostics belong to stderr.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(windows)]
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
#[cfg(windows)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(windows)]
use tokio::process::Command;
#[cfg(windows)]
use tokio::time::timeout;

#[cfg(windows)]
use crate::platform;
use crate::util::{err, Result};

const MAX_SESSION_BYTES: usize = 128;
const MAX_SESSION_TOKEN_BYTES: usize = 171;
#[cfg(windows)]
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Decode the only value accepted from the restricted SSH command surface.
pub fn decode_session_token(token: &str) -> Result<String> {
    // Reject oversized input before asking the decoder to allocate its output.
    if token.len() > MAX_SESSION_TOKEN_BYTES {
        return Err(err("relay session selector is too long"));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| err("relay session selector is not base64url without padding"))?;
    if decoded.is_empty() || decoded.len() > MAX_SESSION_BYTES {
        return Err(err("relay session selector must be 1..=128 decoded bytes"));
    }
    let session =
        String::from_utf8(decoded).map_err(|_| err("relay session selector is not valid UTF-8"))?;
    if session.chars().any(char::is_control) {
        return Err(err("relay session selector contains a control character"));
    }
    Ok(session)
}

/// Parse only `--session-b64=TOKEN`; a separate argument or any other option
/// is intentionally not part of the relay command contract.
pub fn parse_args(args: &[String]) -> Result<Option<String>> {
    match args {
        [] => Ok(None),
        [arg] if arg.starts_with("--session-b64=") => {
            decode_session_token(&arg["--session-b64=".len()..]).map(Some)
        }
        [_] => Err(err("usage: herdr-mirror relay [--session-b64=TOKEN]")),
        _ => Err(err("usage: herdr-mirror relay [--session-b64=TOKEN]")),
    }
}

/// Construct argv for the trusted child. Keeping this as data, rather than a
/// command string, is the important no-shell property of the relay boundary.
pub fn herdr_status_argv(session: Option<&str>) -> Vec<String> {
    let mut argv = Vec::with_capacity(if session.is_some() { 4 } else { 2 });
    if let Some(session) = session {
        argv.extend(["--session".to_string(), session.to_string()]);
    }
    argv.extend(["status".to_string(), "--json".to_string()]);
    argv
}

/// Validate the socket value before it reaches the named-pipe mapper. The
/// status response is local, but it is still untrusted data at this boundary.
pub fn validate_socket_path(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() {
        return Err(err("herdr status returned an empty socket path"));
    }
    if raw.contains('\0') || raw.contains('\r') || raw.contains('\n') {
        return Err(err("herdr socket path contains CR, LF, or NUL"));
    }
    if raw.chars().any(char::is_control) {
        return Err(err("herdr socket path contains a control character"));
    }
    if !crate::util::is_absolute_windows_path(raw) {
        return Err(err("herdr socket path is not an absolute Windows path"));
    }
    Ok(PathBuf::from(raw))
}

fn status_socket(status: &serde_json::Value) -> Result<PathBuf> {
    if status.pointer("/server/running").and_then(|v| v.as_bool()) != Some(true) {
        return Err(err("herdr server is not running"));
    }
    let raw = status
        .pointer("/server/socket")
        .and_then(|v| v.as_str())
        .ok_or_else(|| err("herdr status returned no socket path"))?;
    validate_socket_path(raw)
}

#[cfg(windows)]
fn trusted_herdr_path() -> Result<PathBuf> {
    // This name is deliberately test-only. A release binary that sees it
    // fails closed instead of turning an environment variable into a trust
    // decision.
    #[cfg(test)]
    if let Some(path) = std::env::var_os("HERDR_MIRROR_TEST_HERDR_EXE") {
        let path = PathBuf::from(path);
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            return Err(err(format!(
                "test herdr.exe is missing: {}",
                path.display()
            )));
        }
        return Ok(path);
    }
    #[cfg(not(test))]
    if std::env::var_os("HERDR_MIRROR_TEST_HERDR_EXE").is_some() {
        return Err(err(
            "HERDR_MIRROR_TEST_HERDR_EXE is accepted only by test builds",
        ));
    }
    let mirror = std::env::current_exe()
        .map_err(|e| err(format!("cannot resolve running herdr-mirror: {e}")))?;
    let parent = mirror
        .parent()
        .ok_or_else(|| err("running herdr-mirror has no parent directory"))?;
    let herdr = parent.join("herdr.exe");
    if !std::fs::metadata(&herdr).is_ok_and(|m| m.is_file()) {
        return Err(err(format!(
            "trusted herdr.exe is missing: {}",
            herdr.display()
        )));
    }
    Ok(herdr)
}

#[cfg(not(windows))]
fn trusted_herdr_path() -> Result<PathBuf> {
    Err(err("herdr-mirror relay is supported only on Windows"))
}

#[cfg(windows)]
async fn query_status(herdr: &Path, session: Option<&str>) -> Result<PathBuf> {
    let output = timeout(
        STATUS_TIMEOUT,
        Command::new(herdr)
            .args(herdr_status_argv(session))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| err("herdr status timed out"))?
    .map_err(|e| err(format!("cannot run trusted herdr.exe: {e}")))?;
    if !output.status.success() {
        return Err(err(format!(
            "herdr status failed: {}",
            bounded_diagnostic(&output.stderr)
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| err(format!("herdr status returned invalid JSON: {e}")))?;
    status_socket(&value)
}

#[cfg(windows)]
fn bounded_diagnostic(bytes: &[u8]) -> String {
    const MAX_DIAGNOSTIC: usize = 4096;
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.len() <= MAX_DIAGNOSTIC {
        text.to_string()
    } else {
        format!("{}...", &text[..MAX_DIAGNOSTIC])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayEvent {
    InputEof,
    InputError,
    PipeEof,
    PipeError,
    StdoutError,
}

#[cfg(windows)]
async fn pump(stream: platform::LocalStream) -> Result<()> {
    let (mut pipe_read, mut pipe_write) = platform::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let input = async {
        let mut buf = [0u8; 32 * 1024];
        loop {
            let n = stdin
                .read(&mut buf)
                .await
                .map_err(|e| (RelayEvent::InputError, e))?;
            if n == 0 {
                return Ok::<RelayEvent, (RelayEvent, std::io::Error)>(RelayEvent::InputEof);
            }
            pipe_write
                .write_all(&buf[..n])
                .await
                .map_err(|e| (RelayEvent::InputError, e))?;
        }
    };

    let output = async {
        let mut buf = [0u8; 32 * 1024];
        loop {
            let n = pipe_read
                .read(&mut buf)
                .await
                .map_err(|e| (RelayEvent::PipeError, e))?;
            if n == 0 {
                return Ok::<RelayEvent, (RelayEvent, std::io::Error)>(RelayEvent::PipeEof);
            }
            stdout
                .write_all(&buf[..n])
                .await
                .map_err(|e| (RelayEvent::StdoutError, e))?;
            stdout
                .flush()
                .await
                .map_err(|e| (RelayEvent::StdoutError, e))?;
        }
    };

    tokio::pin!(input);
    tokio::pin!(output);
    let first = tokio::select! {
        event = &mut input => event,
        event = &mut output => event,
    };
    match first {
        Ok(RelayEvent::InputEof) => match timeout(DRAIN_TIMEOUT, &mut output).await {
            Ok(Ok(RelayEvent::PipeEof)) => Ok(()),
            Ok(Ok(_)) | Ok(Err(_)) => Err(err("relay pipe closed while draining")),
            Err(_) => Ok(()),
        },
        Ok(event) => match event {
            RelayEvent::PipeEof => Ok(()),
            RelayEvent::InputEof => Ok(()),
            RelayEvent::InputError => Err(err("relay stdin or pipe write failed")),
            RelayEvent::PipeError => Err(err("relay pipe read failed")),
            RelayEvent::StdoutError => Err(err("relay stdout write failed")),
        },
        Err((event, _)) => {
            Err(err(match event {
                RelayEvent::InputError => "relay stdin or pipe write failed",
                RelayEvent::PipeError => "relay pipe read failed",
                RelayEvent::StdoutError => "relay stdout write failed",
                RelayEvent::InputEof | RelayEvent::PipeEof => "relay stopped",
            }))
        }
    }
}

#[cfg(windows)]
pub async fn cmd(args: &[String]) -> Result<()> {
    let session = parse_args(args)?;
    let herdr = trusted_herdr_path()?;
    let socket = query_status(&herdr, session.as_deref()).await?;
    // `platform::connect` owns the one and only pipe_name conversion. Passing
    // the status path directly avoids the common "prefix twice" failure.
    let stream = platform::connect(&socket)
        .await
        .map_err(|e| err(format!("cannot connect to herdr named pipe: {e}")))?;
    pump(stream).await
}

#[cfg(not(windows))]
pub async fn cmd(args: &[String]) -> Result<()> {
    let _ = parse_args(args)?;
    let _ = trusted_herdr_path()?;
    unreachable!("trusted_herdr_path always errors on non-Windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn selector_decoding_accepts_url_safe_utf8_without_controls() {
        let token = URL_SAFE_NO_PAD.encode("team/é".as_bytes());
        assert_eq!(decode_session_token(&token).unwrap(), "team/é");
    }

    #[test]
    fn selector_decoding_rejects_padding_controls_invalid_utf8_and_oversize() {
        assert!(decode_session_token("YQ==").is_err());
        assert!(decode_session_token(&URL_SAFE_NO_PAD.encode(b"bad\nname")).is_err());
        assert!(decode_session_token("__8").is_err());
        assert!(decode_session_token(&URL_SAFE_NO_PAD.encode([b'a'; 129])).is_err());
    }

    #[test]
    fn status_argv_is_direct_and_session_is_optional() {
        assert_eq!(herdr_status_argv(None), ["status", "--json"]);
        assert_eq!(
            herdr_status_argv(Some("a; & not shell")),
            ["--session", "a; & not shell", "status", "--json"]
        );
    }

    #[test]
    fn socket_validation_rejects_injection_and_relative_values() {
        assert!(validate_socket_path(r"C:\Users\me\herdr.sock").is_ok());
        assert!(
            validate_socket_path(format!("C:\\Users\\me\\herdr{}sock", '\n').as_str()).is_err()
        );
        assert!(validate_socket_path("C:\\Users\\me\\herdr\0.sock").is_err());
        assert!(validate_socket_path(r"herdr.sock").is_err());
    }

    #[test]
    fn status_requires_a_running_server_and_socket() {
        let good = serde_json::json!({
            "server": {"running": true, "socket": r"C:\Users\me\herdr.sock"}
        });
        assert!(status_socket(&good).is_ok());
        assert!(status_socket(&serde_json::json!({
            "server": {"running": false, "socket": r"C:\Users\me\herdr.sock"}
        }))
        .is_err());
        assert!(status_socket(&serde_json::json!({
            "server": {"running": true, "socket": "relative.sock"}
        }))
        .is_err());
        assert!(status_socket(&serde_json::json!({
            "server": {"running": true}
        }))
        .is_err());
    }

}
