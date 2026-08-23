// Client for the herdr JSON socket API (HERDR_SOCKET_PATH protocol).
// Works against any unix socket path — the local server directly, or a remote
// server's socket forwarded over ssh.
//
// Connection semantics (verified against preview 2026-06-30): the server
// serves ONE request per connection and closes after the response. The only
// held connection is events.subscribe, which acks with subscription_started
// and then pushes {event, data} envelopes until either side closes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use crate::util::{err, Result};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub enum IpcStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    WinUnix(tokio::net::TcpStream), // std::os::windows::net::UnixStream wrapped in tokio or TCP fallback
    #[cfg(windows)]
    NamedPipe(tokio::net::windows::named_pipe::NamedPipeClient),
    Tcp(tokio::net::TcpStream),
}

impl IpcStream {
    pub async fn connect(path: &Path) -> Result<IpcStream> {
        let path_str = path.to_string_lossy();
        if let Ok(addr) = path_str.parse::<std::net::SocketAddr>() {
            let stream = tokio::net::TcpStream::connect(addr).await?;
            return Ok(IpcStream::Tcp(stream));
        }
        #[cfg(windows)]
        {
            if let Ok(content) = std::fs::read_to_string(path) {
                let content = content.trim().to_string();
                if let Ok(addr) = content.parse::<std::net::SocketAddr>() {
                    if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                        return Ok(IpcStream::Tcp(stream));
                    }
                }
                if let Ok(stream) = tokio::net::TcpStream::connect(&content).await {
                    return Ok(IpcStream::Tcp(stream));
                }
                if content.starts_with(r"\\.\pipe\") {
                    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(&content)?;
                    return Ok(IpcStream::NamedPipe(client));
                }
            }

            if path_str.starts_with(r"\\.\pipe\") {
                let client = tokio::net::windows::named_pipe::ClientOptions::new().open(path_str.as_ref())?;
                return Ok(IpcStream::NamedPipe(client));
            }

            // herdr on Windows exposes its API socket as a named pipe whose
            // name is \\.\pipe\ + the full absolute file path. Try this before
            // the AF_UNIX fallback, which always fails for herdr.sock paths.
            {
                let abs = path.to_string_lossy().replace('/', "\\");
                let pipe_name = format!(r"\\.\pipe\{abs}");
                if let Ok(client) = tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe_name) {
                    return Ok(IpcStream::NamedPipe(client));
                }
            }

            let path_str_norm = path.to_string_lossy();
            let path_variants = [
                path_str_norm.to_string(),
                path_str_norm.replace('/', "\\"),
                format!("\\\\?\\{}", path_str_norm.replace('/', "\\")),
            ];

            let win_socket = {
                use windows_sys::Win32::Networking::WinSock::{
                    closesocket, connect, socket, WSAStartup, AF_UNIX, SOCK_STREAM, WSADATA,
                };
                use std::os::windows::io::FromRawSocket;

                let mut wsa_data: WSADATA = unsafe { std::mem::zeroed() };
                unsafe { WSAStartup(0x0202, &mut wsa_data) };

                let mut connected_stream = None;
                for p in &path_variants {
                    let sock_fd = unsafe { socket(AF_UNIX as i32, SOCK_STREAM as i32, 0) };
                    if sock_fd == !0 {
                        continue;
                    }
                    #[repr(C)]
                    struct SOCKADDR_UN_WIN {
                        sun_family: u16,
                        sun_path: [u8; 108],
                    }
                    let mut addr: SOCKADDR_UN_WIN = unsafe { std::mem::zeroed() };
                    addr.sun_family = AF_UNIX as u16;
                    let bytes = p.as_bytes();
                    let len = bytes.len().min(addr.sun_path.len() - 1);
                    for i in 0..len {
                        addr.sun_path[i] = bytes[i];
                    }
                    addr.sun_path[len] = 0;
                    let sockaddr_len = std::mem::size_of::<SOCKADDR_UN_WIN>() as i32;
                    let ret = unsafe {
                        connect(
                            sock_fd,
                            &addr as *const _ as *const windows_sys::Win32::Networking::WinSock::SOCKADDR,
                            sockaddr_len,
                        )
                    };
                    if ret == 0 {
                        let std_tcp = unsafe { std::net::TcpStream::from_raw_socket(sock_fd as _) };
                        let _ = std_tcp.set_nonblocking(true);
                        connected_stream = Some(std_tcp);
                        break;
                    } else {
                        unsafe { closesocket(sock_fd) };
                    }
                }
                connected_stream
            };

            if let Some(std_tcp) = win_socket {
                let stream = tokio::net::TcpStream::from_std(std_tcp)?;
                return Ok(IpcStream::WinUnix(stream));
            }

            return Err(err(format!("cannot connect to windows AF_UNIX socket at {}", path.display())));
        }
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(path).await?;
            return Ok(IpcStream::Unix(stream));
        }
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            IpcStream::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            IpcStream::WinUnix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            IpcStream::NamedPipe(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            IpcStream::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            #[cfg(unix)]
            IpcStream::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            IpcStream::WinUnix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            IpcStream::NamedPipe(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            IpcStream::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            IpcStream::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            IpcStream::WinUnix(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            IpcStream::NamedPipe(s) => std::pin::Pin::new(s).poll_flush(cx),
            IpcStream::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            IpcStream::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            IpcStream::WinUnix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            IpcStream::NamedPipe(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            IpcStream::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub event: String,
    pub data: Value,
}

#[derive(Clone)]
pub struct ApiClient {
    socket_path: PathBuf,
}

impl ApiClient {
    /// Connect-check the socket (one ping round-trip), then hand back a client.
    pub async fn connect(socket_path: &Path) -> Result<ApiClient> {
        let client = ApiClient { socket_path: socket_path.to_path_buf() };
        let _ = client.request("ping", json!({})).await;
        Ok(client)
    }

    /// One request on a fresh connection; the server closes after responding.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        timeout(REQUEST_TIMEOUT, self.request_inner(method, params))
            .await
            .map_err(|_| err(format!("api timeout: {method}")))?
    }

    pub async fn request_t<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let v = self.request(method, params).await?;
        serde_json::from_value(v).map_err(|e| err(format!("{method}: bad response shape: {e}")))
    }

    async fn request_inner(&self, method: &str, params: Value) -> Result<Value> {
        let stream_res = timeout(CONNECT_TIMEOUT, IpcStream::connect(&self.socket_path)).await;
        let mut stream = match stream_res {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(err(format!("api connect timeout: {}", self.socket_path.display()))),
        };
        let (read, mut write) = tokio::io::split(&mut stream);
        let id = format!("mirror_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let line = serde_json::to_string(&json!({ "id": id, "method": method, "params": params }))? + "\n";
        write.write_all(line.as_bytes()).await?;
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("id").and_then(|v| v.as_str()) != Some(id.as_str()) {
                continue;
            }
            if let Some(e) = msg.get("error") {
                let text = e
                    .get("message")
                    .or_else(|| e.get("code"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(err(format!("{method}: {text}")));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
        Err(err(format!("api closed before response: {method}")))
    }

    /// Held connection pushing events. Pull with `EventStream::next()`; a
    /// `None` means the stream dropped (resubscribe from the caller).
    pub async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<EventStream> {
        let stream = timeout(CONNECT_TIMEOUT, IpcStream::connect(&self.socket_path))
            .await
            .map_err(|_| err(format!("api connect timeout: {}", self.socket_path.display())))??;
        let (read, write) = tokio::io::split(stream);
        let mut write = write;
        let id = format!("mirror_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let line = serde_json::to_string(
            &json!({ "id": id, "method": "events.subscribe", "params": { "subscriptions": subscriptions } }),
        )? + "\n";
        write.write_all(line.as_bytes()).await?;
        let mut lines = BufReader::new(read).lines();
        // first line is the ack (or an error)
        let ack = timeout(Duration::from_secs(10), lines.next_line())
            .await
            .map_err(|_| err("subscribe ack timeout"))??
            .ok_or_else(|| err("subscribe: stream closed before ack"))?;
        let msg: Value = serde_json::from_str(&ack)?;
        if let Some(e) = msg.get("error") {
            let text = e.get("message").and_then(|v| v.as_str()).unwrap_or("subscribe failed");
            return Err(err(text.to_string()));
        }
        Ok(EventStream { lines, _write: write })
    }
}

pub struct EventStream {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<IpcStream>>>,
    _write: tokio::io::WriteHalf<IpcStream>, // keeps the connection open
}

impl EventStream {
    /// Next event; `None` when the stream has dropped.
    pub async fn next(&mut self) -> Option<EventEnvelope> {
        loop {
            match self.lines.next_line().await {
                Ok(Some(line)) => {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(event) = msg.get("event").and_then(|v| v.as_str()) {
                        return Some(EventEnvelope {
                            event: event.to_string(),
                            data: msg.get("data").cloned().unwrap_or(Value::Null),
                        });
                    }
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }
}
