//! Operating-system boundaries used by the control plane.
//!
//! The herdr JSON protocol is identical on Unix sockets and Windows named
//! pipes. Keeping that distinction here prevents the API and relay code from
//! accumulating per-platform branches.

use std::io;
use std::path::Path;
use std::process::Command;

use fs2::FileExt;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait LocalIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> LocalIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub type LocalStream = Box<dyn LocalIo>;
pub type LocalReadHalf = tokio::io::ReadHalf<LocalStream>;
pub type LocalWriteHalf = tokio::io::WriteHalf<LocalStream>;

#[cfg(unix)]
pub async fn connect(path: &Path) -> io::Result<LocalStream> {
    Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
}

/// Map a filesystem path onto herdr's Windows named-pipe name.
///
/// herdr exposes its API socket as `\\.\pipe\` followed by the absolute socket
/// path it reports (for example `C:\Users\me\AppData\Roaming\herdr\herdr.sock`).
/// The relay endpoints use the same convention, so a single translation keeps
/// client and listener in agreement. A value that already names a pipe is left
/// untouched.
#[cfg(windows)]
pub fn pipe_name(path: &Path) -> std::ffi::OsString {
    use std::ffi::OsString;

    const PIPE_PREFIX: &str = r"\\.\pipe\";
    let raw = path.as_os_str().to_string_lossy();
    if raw.starts_with(PIPE_PREFIX) {
        path.as_os_str().to_os_string()
    } else {
        OsString::from(format!("{PIPE_PREFIX}{raw}"))
    }
}

#[cfg(windows)]
pub async fn connect(path: &Path) -> io::Result<LocalStream> {
    use tokio::net::windows::named_pipe::ClientOptions;

    Ok(Box::new(ClientOptions::new().open(pipe_name(path))?))
}

#[cfg(unix)]
pub struct LocalListener {
    inner: tokio::net::UnixListener,
}

#[cfg(unix)]
impl LocalListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: tokio::net::UnixListener::bind(path)?,
        })
    }

    pub async fn accept(&mut self) -> io::Result<LocalStream> {
        let (stream, _) = self.inner.accept().await?;
        Ok(Box::new(stream))
    }
}

#[cfg(windows)]
pub struct LocalListener {
    name: std::ffi::OsString,
    inner: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl LocalListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let name = pipe_name(path);
        let mut options = ServerOptions::new();
        // Refuse to adopt an endpoint another (possibly live) process owns.
        // Follow-on instances created in `accept` must NOT set this flag.
        options.first_pipe_instance(true);
        let inner = options.create(&name)?;
        Ok(Self { name, inner })
    }

    pub async fn accept(&mut self) -> io::Result<LocalStream> {
        use tokio::net::windows::named_pipe::ServerOptions;

        self.inner.connect().await?;
        // Publish a fresh instance before handing the connected one to the
        // caller, so the next client never races into ERROR_PIPE_BUSY.
        let next = ServerOptions::new().create(&self.name)?;
        let connected = std::mem::replace(&mut self.inner, next);
        Ok(Box::new(connected))
    }
}

pub fn split(stream: LocalStream) -> (LocalReadHalf, LocalWriteHalf) {
    tokio::io::split(stream)
}

pub fn remove_endpoint(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
    #[cfg(windows)]
    {
        // Named pipes are kernel objects, not directory entries. The server
        // handle owns their lifetime; there is nothing to unlink.
        let _ = path;
        Ok(())
    }
}

/// Whether a local endpoint already exists. Named pipes on Windows are kernel
/// objects whose only reliable probe is `connect`, so this is Unix-only; the
/// Windows control plane proves liveness by connecting.
#[cfg(not(windows))]
pub fn endpoint_exists(path: &Path) -> bool {
    path.exists()
}

pub fn set_private_endpoint(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

pub fn try_lock(file: &std::fs::File) -> io::Result<()> {
    file.try_lock_exclusive()
}

pub fn unlock(file: &std::fs::File) -> io::Result<()> {
    file.unlock()
}

pub fn configure_detached_command(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        // The daemon is a console binary with all three stdio streams already
        // redirected to files; without this, spawning it from herdr flashes a
        // console window (or fails when the parent has no console at all).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Own a Windows transport and all descendants it creates. Closing a job with
/// KILL_ON_JOB_CLOSE is the reliable cleanup path for ssh ProxyCommand trees;
/// killing only ssh leaves those helpers attached to the remote session.
#[cfg(windows)]
pub struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for ProcessTree {}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(windows)]
pub fn attach_process_tree(pid: u32) -> io::Result<ProcessTree> {
    use std::ptr::null_mut;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    unsafe {
        let job = CreateJobObjectW(null_mut(), null_mut());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut limits as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let _ = windows_sys::Win32::Foundation::CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            let _ = windows_sys::Win32::Foundation::CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        let assigned = AssignProcessToJobObject(job, process);
        let _ = windows_sys::Win32::Foundation::CloseHandle(process);
        if assigned == 0 {
            let _ = windows_sys::Win32::Foundation::CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        Ok(ProcessTree { job })
    }
}

#[cfg(windows)]
pub fn terminate_process_tree(tree: ProcessTree) {
    unsafe {
        let _ = windows_sys::Win32::System::JobObjects::TerminateJobObject(tree.job, 1);
    }
    drop(tree);
}

pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if handle.is_null() {
                return false;
            }
            CloseHandle(handle);
            true
        }
    }
}

pub fn poke_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("daemon.poke")
}

/// Wake the daemon's sync loop. Unix delivers SIGUSR1 (what `PokeWaiter`
/// listens for); Windows has no portable signal, so it drops a `.poke` marker
/// the polling waiter consumes. `pid` is unused on Windows.
pub fn request_poke(state_dir: &Path, pid: i32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = state_dir;
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGUSR1) };
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = pid;
        std::fs::write(poke_path(state_dir), "poke")
    }
}

/// Ask the daemon to stop. Unix signals SIGTERM; Windows drops a `.stop` marker
/// the polling `ShutdownWaiter` consumes. `pid` is unused on Windows.
pub fn request_stop(state_dir: &Path, pid: i32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = state_dir;
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = pid;
        std::fs::write(state_dir.join("daemon.stop"), "stop")
    }
}

/// Drop pending `.stop`/`.poke` markers left by a previous lifecycle command.
/// Called on the resume path: a stale `.stop` would otherwise make a freshly
/// spawned daemon exit before doing any work.
pub fn clear_lifecycle_requests(state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join("daemon.stop"));
    let _ = std::fs::remove_file(poke_path(state_dir));
}

pub struct PokeWaiter {
    #[cfg_attr(not(windows), allow(dead_code))]
    state_dir: std::path::PathBuf,
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}

impl PokeWaiter {
    pub fn new(state_dir: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
            Ok(Self {
                state_dir: state_dir.to_path_buf(),
                signal,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                state_dir: state_dir.to_path_buf(),
            })
        }
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let _ = self.signal.recv().await;
        }
        #[cfg(windows)]
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if std::fs::remove_file(poke_path(&self.state_dir)).is_ok() {
                break;
            }
        }
    }
}

pub struct ShutdownWaiter {
    #[cfg_attr(not(windows), allow(dead_code))]
    state_dir: std::path::PathBuf,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl ShutdownWaiter {
    pub fn new(state_dir: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                state_dir: state_dir.to_path_buf(),
                terminate: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                )?,
                interrupt: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::interrupt(),
                )?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                state_dir: state_dir.to_path_buf(),
            })
        }
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.terminate.recv() => {},
                _ = self.interrupt.recv() => {},
            }
        }
        #[cfg(windows)]
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if std::fs::remove_file(self.state_dir.join("daemon.stop")).is_ok() {
                break;
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn socket_paths_map_to_herdr_pipe_names() {
        assert_eq!(
            pipe_name(Path::new(r"C:\Users\me\AppData\Roaming\herdr\herdr.sock")),
            std::ffi::OsString::from(r"\\.\pipe\C:\Users\me\AppData\Roaming\herdr\herdr.sock")
        );
        assert_eq!(
            pipe_name(Path::new(r"\\.\pipe\already-a-pipe")),
            std::ffi::OsString::from(r"\\.\pipe\already-a-pipe")
        );
    }

    /// Live proof that the control plane speaks to the installed herdr over its
    /// named pipe. Off by default: it needs a running local herdr and pings the
    /// real server, so run it explicitly with
    /// `cargo test -- --ignored connects_to_the_live_herdr_pipe`.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires a running local herdr and HERDR_SOCKET_PATH"]
    async fn connects_to_the_live_herdr_pipe() {
        let path = std::env::var_os("HERDR_SOCKET_PATH")
            .expect("HERDR_SOCKET_PATH must point at herdr.sock");
        let client = crate::api::ApiClient::connect(Path::new(&path))
            .await
            .expect("connect + ping over the herdr named pipe");
        let pong = client.request("ping", serde_json::json!({})).await.unwrap();
        assert_eq!(pong["type"], "pong");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_round_trip_uses_the_same_byte_stream_contract() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let name = std::path::PathBuf::from(format!(
            r"\\.\pipe\herdr-mirror-test-{}",
            std::process::id()
        ));
        let mut listener = LocalListener::bind(&name).unwrap();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (read, mut write) = split(stream);
            let mut lines = BufReader::new(read).lines();
            let request = lines.next_line().await.unwrap().unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_str().unwrap();
            write
                .write_all(format!("{{\"id\":\"{id}\",\"result\":{{\"pong\":true}}}}\n").as_bytes())
                .await
                .unwrap();
        });

        let stream = connect(&name).await.unwrap();
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        write
            .write_all(b"{\"id\":\"test\",\"method\":\"ping\",\"params\":{}}\n")
            .await
            .unwrap();
        let mut response = String::new();
        read.read_line(&mut response).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(response.trim()).unwrap()["result"]["pong"],
            true
        );
        server.await.unwrap();
    }
}
