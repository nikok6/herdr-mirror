use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::pane::ContainerArg;
use crate::remote::SSH_COMMON_OPTS;
use crate::util::{err, Result};

const REMOTE_DIR: &str = ".cache/herdr-mirror/pastes";

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(20);

pub enum Outcome {
    Pasted(String),
    NoImage,
    Failed(String),
}

pub async fn clipboard_to_remote(
    ssh_target: &str,
    ctl_path: Option<&str>,
    container: Option<&ContainerArg>,
) -> Outcome {
    let png = match read_clipboard_image().await {
        Some(b) if !b.is_empty() => b,
        _ => return Outcome::NoImage,
    };
    match upload(&png, ssh_target, ctl_path, container).await {
        Ok(path) => Outcome::Pasted(path),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

async fn read_clipboard_image() -> Option<Vec<u8>> {
    if cfg!(target_os = "macos") {
        if let Some(bytes) = run_capture("pngpaste", &["-"]).await {
            return Some(bytes);
        }
        let out = run_capture("osascript", &["-e", "the clipboard as «class PNGf»"]).await?;
        parse_osascript_data(&String::from_utf8_lossy(&out))
    } else {
        if let Some(bytes) = run_capture("wl-paste", &["-t", "image/png"]).await {
            return Some(bytes);
        }
        run_capture("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]).await
    }
}

async fn run_capture(bin: &str, args: &[&str]) -> Option<Vec<u8>> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = timeout(CLIPBOARD_TIMEOUT, child.wait_with_output()).await.ok()?.ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

fn parse_osascript_data(s: &str) -> Option<Vec<u8>> {
    let hex = s.trim().strip_prefix("«data PNGf")?.strip_suffix('»')?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let digits = hex.as_bytes();
    for pair in digits.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
    }
    Some(bytes)
}

async fn upload(
    png: &[u8],
    ssh_target: &str,
    ctl_path: Option<&str>,
    container: Option<&ContainerArg>,
) -> Result<String> {
    let name = format!(
        "paste-{}-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        std::process::id()
    );
    let cmd = format!("mkdir -p ~/{REMOTE_DIR} && cat > ~/{REMOTE_DIR}/{name} && echo \"$HOME\"");
    let mut c = match container {
        Some(ct) => {
            let ids = crate::docker::resolve(&ct.docker_bin, &ct.kind).await?;
            let id = ids.into_iter().next().ok_or_else(|| err("container not found"))?;
            let mut c = Command::new(&ct.docker_bin);
            c.args(["exec", "-i", &id, "sh", "-c", &cmd]);
            c
        }
        None => {
            let mut c = Command::new("ssh");
            if let Some(path) = ctl_path {
                c.arg("-S").arg(path);
            }
            c.args(SSH_COMMON_OPTS).arg(ssh_target).arg(&cmd);
            c
        }
    };
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| err("no child stdin"))?;
    stdin.write_all(png).await?;
    drop(stdin);
    let out = timeout(UPLOAD_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| err("upload timed out"))??;
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr);
        let tail = tail.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("upload failed");
        return Err(err(tail.to_string()));
    }
    let home = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if home.is_empty() || !home.starts_with('/') {
        return Err(err("remote gave no home directory back"));
    }
    Ok(format!("{home}/{REMOTE_DIR}/{name}"))
}

pub fn bracketed(path: &str) -> Vec<u8> {
    let mut b: Vec<u8> = b"\x1b[200~".to_vec();
    b.extend_from_slice(path.as_bytes());
    b.extend_from_slice(b"\x1b[201~");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osascript_data_literal_decodes() {
        assert_eq!(parse_osascript_data("«data PNGf48656c6c6f»\n"), Some(b"Hello".to_vec()));
        let png = parse_osascript_data("«data PNGf89504e470d0a1a0a»").unwrap();
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn osascript_data_rejects_garbage() {
        assert_eq!(parse_osascript_data("execution error: ..."), None);
        assert_eq!(parse_osascript_data("«data PNGf»"), None);
        assert_eq!(parse_osascript_data("«data PNGf123»"), None);
        assert_eq!(parse_osascript_data("«data PNGfzz»"), None);
        assert_eq!(parse_osascript_data("«data TIFF4865»"), None);
    }

    #[test]
    fn bracketed_paste_wraps_verbatim() {
        let b = bracketed("/home/u/.cache/herdr-mirror/pastes/p.png");
        assert!(b.starts_with(b"\x1b[200~"));
        assert!(b.ends_with(b"\x1b[201~"));
        assert_eq!(&b[6..b.len() - 6], b"/home/u/.cache/herdr-mirror/pastes/p.png");
    }
}
