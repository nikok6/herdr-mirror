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
    upload_named(png, Some("png"), ssh_target, ctl_path, container).await
}

/// The name is generated, never taken from the source: a dropped file's own
/// name could carry anything, and this string is interpolated into a remote
/// shell command. The extension is the one thing we keep, sanitised to
/// alphanumerics so it cannot smuggle a quote or a path separator.
async fn upload_named(
    png: &[u8],
    ext: Option<&str>,
    ssh_target: &str,
    ctl_path: Option<&str>,
    container: Option<&ContainerArg>,
) -> Result<String> {
    let ext = ext
        .map(|e| e.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect::<String>())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "bin".into());
    // The counter, not just the clock: a multi-file drop can finish two
    // uploads inside one millisecond, and a collision would silently overwrite
    // the first file and paste two identical paths.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "paste-{}-{}-{}.{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let cmd = format!("mkdir -p ~/{REMOTE_DIR} && cat > ~/{REMOTE_DIR}/{name} && echo \"$HOME\"");
    let mut c = remote_command(&cmd, ssh_target, ctl_path, container).await?;
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

/// One `sh -c` on the far end, over whichever transport this pane uses.
async fn remote_command(
    cmd: &str,
    ssh_target: &str,
    ctl_path: Option<&str>,
    container: Option<&ContainerArg>,
) -> Result<Command> {
    Ok(match container {
        Some(ct) => {
            let ids = crate::docker::resolve(&ct.docker_bin, &ct.kind).await?;
            let id = ids.into_iter().next().ok_or_else(|| err("container not found"))?;
            let mut c = Command::new(&ct.docker_bin);
            c.args(["exec", "-i", &id, "sh", "-c", cmd]);
            c
        }
        None => {
            let mut c = Command::new("ssh");
            if let Some(path) = ctl_path {
                c.arg("-S").arg(path);
            }
            c.args(SSH_COMMON_OPTS).arg(ssh_target).arg(cmd);
            c
        }
    })
}

/// Frame `body` as a bracketed paste for the remote pty.
///
/// The markers are what tell the remote app "this arrived as a paste, not as
/// keystrokes" — without them every newline in the body reads as Enter, so a
/// multi-line paste submits itself a line at a time. herdr only wraps a paste
/// when the pane's app has enabled DECSET 2004, and the streamer enables it
/// (see `pane::run`), so a paste reaching us is framed and has to leave framed
/// too. Both ends of the payload are ours, so this is the one place that knows
/// the byte sequences besides `split_paste`.
pub fn bracketed_bytes(body: &[u8]) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::with_capacity(body.len() + 12);
    b.extend_from_slice(b"\x1b[200~");
    b.extend_from_slice(body);
    b.extend_from_slice(b"\x1b[201~");
    b
}

pub fn bracketed(path: &str) -> Vec<u8> {
    bracketed_bytes(path.as_bytes())
}

/// Cap per dropped file. Enforced on the READ, not just the stat, so a file
/// that grows underneath us — or a pseudo-file reporting zero length and
/// streaming forever — cannot get past it. Generous enough for screenshots,
/// logs and PDFs; small enough that a stray video drop fails in a sentence.
pub const MAX_DROP_BYTES: u64 = 32 * 1024 * 1024;

/// What to do with a payload that looked like a file drop.
pub struct DropResult {
    /// text to paste in place of the payload; None pastes nothing
    pub text: Option<String>,
    /// forward the original payload untouched (every path already exists on
    /// the remote, so the user meant those files, not copies of ours)
    pub unchanged: bool,
    /// message for the pane hint
    pub error: Option<String>,
}

/// Split a dropped-file payload into paths.
///
/// Captured from a real terminal: absolute paths, single-space separated,
/// spaces inside a name backslash-escaped, and no trailing newline. Some
/// terminals quote instead of escaping, which herdr also handles upstream
/// (`strip_matching_path_quotes`), so both forms are accepted here.
///
/// Returns None unless every token is an absolute path to an existing regular
/// file. Requiring *all* of them is what keeps an ordinary paste that merely
/// mentions a path from being treated as a drop.
pub fn dropped_paths(text: &str) -> Option<Vec<std::path::PathBuf>> {
    let text = text.trim_matches(|c: char| c == '\r' || c == '\n');
    if text.is_empty() {
        return None;
    }
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            // the escape is the terminal's, not the shell's: the next char is
            // literal whatever it is. A trailing lone backslash is kept rather
            // than rejecting the payload, matching herdr's unescape.
            '\\' => match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            },
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            _ if Some(ch) == quote => quote = None,
            ' ' if quote.is_none() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    let paths: Vec<std::path::PathBuf> =
        tokens.into_iter().map(std::path::PathBuf::from).collect();
    let all_files = paths
        .iter()
        .all(|p| p.is_absolute() && std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false));
    (!paths.is_empty() && all_files).then_some(paths)
}

/// Which of `paths` already exist at the same absolute path on the remote.
///
/// This is what separates a drop from an ordinary paste of a path. `/etc/hosts`
/// exists on both machines, so the user meant the remote's copy and we must not
/// substitute ours; a screenshot from ~/Desktop does not exist there, so it has
/// to be uploaded to mean anything. One round trip answers it for the whole
/// payload.
async fn remote_has_all(
    paths: &[std::path::PathBuf],
    ssh_target: &str,
    ctl_path: Option<&str>,
    container: Option<&ContainerArg>,
) -> Result<bool> {
    let quoted: Vec<String> = paths
        .iter()
        .map(|p| crate::pane::sh_quote(&p.display().to_string()))
        .collect();
    let cmd = format!(
        "for p in {}; do [ -e \"$p\" ] && echo 1 || echo 0; done",
        quoted.join(" ")
    );
    let mut c = remote_command(&cmd, ssh_target, ctl_path, container).await?;
    let out = timeout(CLIPBOARD_TIMEOUT, c.stdin(Stdio::null()).output())
        .await
        .map_err(|_| err("remote existence check timed out"))??;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    // a short or garbled answer means "don't know", which must not read as
    // "all present" — that would skip the upload and paste useless local paths
    Ok(lines.len() == paths.len() && lines.iter().all(|l| *l == "1"))
}

/// Upload each dropped file and hand back the remote paths.
///
/// All-or-nothing on the *decision* (if any path is missing remotely, every
/// file is uploaded) so the pasted list is uniform and needs no re-escaping:
/// generated names contain nothing a shell would split on.
pub async fn files_to_remote(
    paths: &[std::path::PathBuf],
    ssh_target: &str,
    ctl_path: Option<&str>,
    container: Option<&ContainerArg>,
) -> DropResult {
    // size-check everything before uploading anything, so a too-large file at
    // the end cannot leave earlier ones orphaned on the remote
    for path in paths {
        match tokio::fs::metadata(path).await {
            Ok(m) if m.len() > MAX_DROP_BYTES => {
                return DropResult {
                    text: None,
                    unchanged: false,
                    error: Some(format!(
                        "{} is {} MB; limit is {} MB",
                        path.display(),
                        m.len() / (1024 * 1024),
                        MAX_DROP_BYTES / (1024 * 1024)
                    )),
                }
            }
            Ok(_) => {}
            Err(e) => {
                return DropResult {
                    text: None,
                    unchanged: false,
                    error: Some(format!("{}: {e}", path.display())),
                }
            }
        }
    }

    match remote_has_all(paths, ssh_target, ctl_path, container).await {
        Ok(true) => {
            return DropResult { text: None, unchanged: true, error: None };
        }
        Ok(false) => {}
        // a failed probe must not block the drop: uploading is the safe guess
        Err(_) => {}
    }

    let mut remote = Vec::new();
    let mut failed = Vec::new();
    for path in paths {
        let bytes = match read_capped(path).await {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        // keep the extension: an agent on the other end usually decides how to
        // read a file from it
        let ext = path.extension().and_then(|e| e.to_str());
        match upload_named(&bytes, ext, ssh_target, ctl_path, container).await {
            Ok(p) => remote.push(p),
            Err(e) => failed.push(format!("{}: {e}", path.display())),
        }
    }

    // partial success still pastes what made it, rather than throwing away
    // files already sitting on the remote
    DropResult {
        text: (!remote.is_empty()).then(|| remote.join(" ")),
        unchanged: false,
        error: (!failed.is_empty()).then(|| failed.join("; ")),
    }
}

/// Read at most `MAX_DROP_BYTES`, off the reactor thread and without trusting
/// the stat: `take` bounds what a growing or endless file can hand back.
async fn read_capped(path: &std::path::Path) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    let read = file.take(MAX_DROP_BYTES + 1).read_to_end(&mut buf).await?;
    if read as u64 > MAX_DROP_BYTES {
        return Err(err(format!("larger than {} MB", MAX_DROP_BYTES / (1024 * 1024))));
    }
    Ok(buf)
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

    /// Real terminal output, captured by dragging onto a pane: absolute paths,
    /// single-space separated, spaces inside a name backslash-escaped, and
    /// notably NO trailing newline.
    #[test]
    fn drop_payload_splits_on_unescaped_spaces_only() {
        let dir = std::env::temp_dir().join(format!("drop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spaced = dir.join("drop test A.png");
        let plain = dir.join("d2.html");
        std::fs::write(&spaced, b"x").unwrap();
        std::fs::write(&plain, b"y").unwrap();

        // one file whose name contains spaces: escaped, must stay one path
        let one = format!("{}", spaced.display()).replace(' ', "\\ ");
        assert_eq!(dropped_paths(&one), Some(vec![spaced.clone()]));

        // two files: the separating space is NOT escaped
        let two = format!("{one} {}", plain.display());
        assert_eq!(dropped_paths(&two), Some(vec![spaced.clone(), plain.clone()]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ordinary_paste_is_never_treated_as_a_drop() {
        // text that isn't paths at all
        assert_eq!(dropped_paths("git status && cargo test"), None);
        assert_eq!(dropped_paths(""), None);
        // a path-shaped string that does not exist
        assert_eq!(dropped_paths("/tmp/definitely-not-here-9182.png"), None);
        // relative paths are not drops (a drop is always absolute)
        assert_eq!(dropped_paths("src/paste.rs"), None);
        // a directory is not a file
        assert_eq!(dropped_paths(&std::env::temp_dir().display().to_string()), None);
    }

    #[test]
    fn one_missing_file_disqualifies_the_whole_paste() {
        // partial matches must not upload: forwarding the text unchanged is
        // the safe outcome, since this is probably prose that mentions a path
        let real = std::env::temp_dir().join(format!("drop-solo-{}.txt", std::process::id()));
        std::fs::write(&real, b"z").unwrap();
        let mixed = format!("{} /tmp/nope-not-real-4471.png", real.display());
        assert_eq!(dropped_paths(&mixed), None);
        std::fs::remove_file(&real).ok();
    }

    #[test]
    fn upload_extension_cannot_escape_the_remote_command() {
        // the filename is interpolated into a remote `sh -c`; only the
        // extension comes from user data, so it must survive nothing but
        // alphanumerics
        let san = |e: Option<&str>| {
            e.map(|e| e.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect::<String>())
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| "bin".into())
        };
        assert_eq!(san(Some("png")), "png");
        assert_eq!(san(Some("tar.gz")), "targz");
        assert_eq!(san(Some("a/b")), "ab");
        assert_eq!(san(Some("'; rm -rf / #")), "rmrf");
        assert_eq!(san(Some("..")), "bin");
        assert_eq!(san(None), "bin");
        assert_eq!(san(Some(&"z".repeat(40))).len(), 16);
    }

    #[test]
    fn quoted_and_backslashed_drops_both_parse() {
        let dir = std::env::temp_dir().join(format!("drop-q-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spaced = dir.join("a b.png");
        std::fs::write(&spaced, b"x").unwrap();

        let esc = spaced.display().to_string().replace(' ', "\\ ");
        assert_eq!(dropped_paths(&esc), Some(vec![spaced.clone()]));
        // some terminals quote instead of escaping
        assert_eq!(dropped_paths(&format!("'{}'", spaced.display())), Some(vec![spaced.clone()]));
        assert_eq!(dropped_paths(&format!("\"{}\"", spaced.display())), Some(vec![spaced.clone()]));

        // a trailing lone backslash is kept, not a reason to reject
        let plain = dir.join("c.png");
        std::fs::write(&plain, b"y").unwrap();
        assert_eq!(dropped_paths(&format!("{}\\", plain.display())), None, "no such file with a trailing backslash");
        std::fs::remove_dir_all(&dir).ok();
    }
}
