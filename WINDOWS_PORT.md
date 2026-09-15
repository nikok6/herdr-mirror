# Windows support

`herdr` itself is not modified by any of this. It is treated as the protocol
endpoint on both sides.

There are two independent directions, and they are worth naming separately
because they fail differently:

- **Windows as client** — `herdr-mirror` runs on Windows and mirrors remote
  Unix hosts. This is the port of the control plane and data plane.
- **Windows as host** — a Unix `herdr-mirror` mirrors a Windows machine's herdr
  over SSH, via `kind = "windows"`. This is the relay.

## Windows as client

### Local IPC

herdr exposes its API at `\\.\pipe\` followed by the absolute socket path it
reports (`HERDR_SOCKET_PATH`, e.g.
`C:\Users\me\AppData\Roaming\herdr\herdr.sock`). `platform::pipe_name` applies
that translation once, for both `connect` and `LocalListener::bind`, so relays
and clients agree. Without it, opening `C:\...\herdr.sock` as a pipe fails and
every control-plane call is dead.

`src/platform.rs` abstracts local byte streams and listeners: Unix sockets on
Unix, named pipes on Windows. `src/api.rs` keeps the JSON protocol unchanged and
connects through it.

### Daemon and process lifecycle

- Unix `flock` is replaced with cross-platform `fs2` file locks.
- Shutdown and poke use `.poke`/`.stop` markers on Windows, since a detached
  process has no console to receive Ctrl+C. Unix keeps signals. `start` clears
  stale markers so a resumed daemon cannot consume an old stop.
- Detached daemon spawn uses `CREATE_NO_WINDOW`.
- Pidfiles record PID *and* process creation time, so PID reuse fails closed.
- Process-tree cleanup uses a Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, covering pane streams and timed-out ssh
  commands including `ProxyCommand` descendants. (On Unix this is a process
  group kill; a plain child kill would leak the ProxyCommand.)

### Terminal

- `term_size()` uses `GetConsoleScreenBufferInfo`, falling back to 80x24 when
  there is no console (CI, pipes).
- `RawMode` enables `ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin and
  `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on stdout, restoring both on drop.
- Unix signals (`SIGWINCH`, `SIGUSR1`, `SIGTERM`, `SIGHUP`, `SIGINT`) are
  `#[cfg(unix)]`. Windows uses `tokio::signal::ctrl_c()` in the same
  `tokio::select!` loop and polls for console size changes; `Msg::Resize` is
  shared by both paths.

### Not ported

- The interactive host picker (`pick.rs`).
- `api_transport = "socket"` is rejected: Windows OpenSSH has no ControlMaster,
  so the exec relay is used and the relay child drops `-S`.
- The docker container transport.

## Windows as host

A Unix client reaches a Windows herdr through `kind = "windows"`. Configuration
is in the README; this section is the design.

`herdr-mirror relay` (`src/relay.rs`) is a stateless stdio-to-named-pipe bridge
run on the Windows machine over an SSH exec channel. The Unix daemon opens a
local Unix relay socket and starts one SSH exec channel per accepted API
connection. API requests and the long-lived events subscription use the same
mechanism. Terminal session streams continue over the existing SSH stdio path,
but their remote commands are built for Windows rather than POSIX.

The relay's trust boundary is deliberately narrow:

- It never accepts an endpoint path. It resolves `herdr.exe` as the sibling of
  its own executable, invokes it with argv (never through a shell), asks for
  `status --json`, and connects only to the socket that reports.
- The only value it accepts from the command line is a URL-safe-no-padding
  base64 session selector: optional, valid UTF-8, no control characters,
  1..=128 decoded bytes.
- After connecting, stdout carries pipe bytes only; diagnostics go to stderr,
  bounded. Pre-connection failures exit nonzero without writing to stdout, so a
  failure can never be mistaken for protocol data.
- Because a Windows duplex named pipe has no verified Win32 half-close here,
  stdin EOF is treated as a logical write-side close: the relay stops reading
  stdin, keeps the pipe handle, and drains pipe-to-stdout for a bounded deadline.
  This preserves the ordinary response-after-EOF case while bounding quiet
  subscription leaks.

Remote command construction on the client side is an explicit matrix rather than
POSIX quoting:

- Status and terminal streams invoke the configured `remote_bin` directly.
- Foreground classification uses two separate calls, not a `;` pipeline.
- CWD policy falls back to the inherited cwd.
- Paste and file upload are disabled, rather than emitting `mkdir`/`cat`/
  `base64` and shell redirection through a wrapper.

`remote_bin` and `relay_bin` are required to be absolute and free of characters
a command wrapper could reinterpret, because they are used as literal command
prefixes. `scripts/herdr-forcecommand.ps1` is an optional wrapper that restricts
a dedicated key to exactly those two command shapes.

## Evidence

Each claim below has a command that can be rerun.

- `cargo test --locked` on Windows — 216 passed, 2 ignored (the ignored ones are
  live integration tests).
- `cargo clippy --all-targets --locked -- -D warnings` on Windows — clean.
- `cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings`
  — clean. This cross-check is load-bearing, not ceremony: a Windows-only run
  cannot see `cfg(unix)` code at all, so Unix-gated mistakes pass Windows CI
  silently.
- `cargo build --release --locked` — produces `target/release/herdr-mirror.exe`.
- `cargo test --locked -- --ignored connects_to_the_live_herdr_pipe` — connects
  to the installed herdr over `\\.\pipe\<HERDR_SOCKET_PATH>` and completes a
  `ping` round-trip. The load-bearing test for the local transport.
- `cargo test --locked -- --ignored relay_reaches_windows_api` on a Unix client
  — API relay against a real Windows host, including repeated SSH exec channels
  over one ControlMaster. Needs `HERDR_MIRROR_IT_WINDOWS_TARGET` and
  `HERDR_MIRROR_IT_WINDOWS_RELAY_BIN`.

Live mirror lifecycle against a Windows host (observe stream, `--always-control`
input, resize, reconnect across a replaced SSH child, and clean shutdown with no
leftover streamer process or temporary files) has been exercised manually; it is
not automated.

## Known gaps

- The plugin manifest invokes `./target/release/herdr-mirror`; confirm herdr
  appends `.exe` on Windows before relying on install-from-release, otherwise
  the manifest needs the `.exe` name on Windows.
- Windows drag/drop path parsing and clipboard behaviour are unvalidated.
- Release assets for `x86_64-pc-windows-msvc` are wired into CI but the upgrade,
  stale-daemon recovery, and named-pipe cleanup paths have not been exercised
  through `herdr plugin install`.
