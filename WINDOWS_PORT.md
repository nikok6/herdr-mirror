# Windows Port

This port adapts `herdr-mirror` only. The installed `herdr` on Windows remains
unchanged and is treated as the protocol endpoint.

## What "control plane" means here

The control plane is the part that talks JSON to the local herdr and to remote
herdr servers, drives the daemon lifecycle, and issues remote actions. It does
not render a terminal. Porting it means:

- local IPC connects to herdr's Windows named pipe and keeps the byte-stream
  contract identical to a Unix socket;
- the relays (ssh exec and docker) listen on their own named pipes;
- file locks, pid liveness, and daemon start/stop work without `libc` signals.

## Evidence path

Each claim has a command that can be rerun on Windows:

- `cargo test --locked` on Windows — 192 passed, 2 ignored (the ignored ones are
  live integration tests).
- `cargo clippy --all-targets --locked -- -D warnings` on Windows — clean.
- `cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings`
  — clean. The port initially left `cfg(unix)` test code without its
  `PermissionsExt` import and an unused `CommandExt` import; a Windows-only
  check cannot see either, so the Unix target must be cross-checked too.
- `cargo build --release --locked` — produces `target/release/herdr-mirror.exe`.
- `cargo test --locked -- --ignored connects_to_the_live_herdr_pipe` — connects
  to the installed herdr over `\\.\pipe\<HERDR_SOCKET_PATH>` and completes a
  `ping` round-trip. This is the load-bearing test for the local transport.
- `herdr-mirror status` — reads config/state and prints host status.

The evidence does not establish terminal rendering, raw input, resize, mouse
forwarding, or the interactive picker. Those are separate claims and remain out
of scope for the first stage.

## Named-pipe mapping

herdr exposes its API at `\\.\pipe\` followed by the absolute socket path it
reports (`HERDR_SOCKET_PATH`, e.g. `C:\Users\me\AppData\Roaming\herdr\herdr.sock`).
`platform::pipe_name` applies that translation once, for both `connect` and
`LocalListener::bind`, so relays and clients agree. Without it, opening
`C:\...\herdr.sock` as a pipe fails and every control-plane call is dead.

## Stages

### Stage 0: Baseline and Sync

Completed. The clone was fast-forwarded with `git pull --ff-only` to upstream
`v0.4.3` (`195398c`).

### Stage 1: Control Plane and Local IPC

Completed and verified in this working tree.

- `src/platform.rs` abstracts local byte streams/listeners: Unix sockets on
  Unix, named pipes on Windows, with the path→pipe-name mapping above.
- `src/api.rs` keeps the JSON protocol unchanged and connects through
  `platform`.
- The ssh and docker relays use `platform::LocalListener`; on Windows they listen
  on pipes instead of TCP loopback.
- Unix `flock` is replaced with cross-platform `fs2` file locks.
- Daemon shutdown/poke use `.poke`/`.stop` markers on Windows (a detached
  process has no console to receive Ctrl+C); Unix keeps signals. Stale markers
  are cleared by `start` so a resumed daemon cannot consume an old stop.
- `ensure_master`/`base_args` are Unix-only paths: Windows OpenSSH has no
  ControlMaster, so `api_transport = "socket"` is rejected and the exec relay is
  used. The relay child also drops `-S` on Windows.
- Detached daemon spawn uses `CREATE_NO_WINDOW`.
- `herdr-plugin.toml`, CI, the release workflow, and `scripts/install.sh` cover
  `x86_64-pc-windows-msvc`.
- Pane and picker commands fail explicitly.

Acceptance: Windows build is warning-free under Clippy, unit tests pass, and the
live named-pipe ping round-trips.

### Stage 2: Windows Data Plane

#### Phase 2A: Shared Data-Plane Compilation (completed)

The shared `src/pane.rs` state machine now compiles and runs on Windows for the
attended `pane <linux-host> <remote-pane> --dump` diagnostic. The Windows-specific
`pane_windows.rs` stub is retained but no longer compiled; the shared file is used
on both platforms.

Platform-specific changes in `src/pane.rs`:

- **Imports**: `tokio::signal::unix::{signal, SignalKind}` gated behind `#[cfg(unix)]`.
- **`term_size()`**: Windows impl uses `GetConsoleScreenBufferInfo` from
  `Win32::System::Console`; falls back to 80×24 when the console API is unavailable
  (CI, pipes).
- **`RawMode`**: Windows impl enables `ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin and
  `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on stdout, restoring original modes on drop.
- **Process-tree cleanup**: Windows transports use a shared job-object helper with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; it covers pane streams and timed-out daemon
  ssh commands, including ProxyCommand descendants.
- **`isolate_session_process_group()`**: Windows path sets `CREATE_NO_WINDOW`
  (0x0800_0000) via `CommandExt::creation_flags`.
- **Signal handling**: Unix signals (`SIGWINCH`, `SIGUSR1`, `SIGTERM`, `SIGHUP`,
  `SIGINT`) gated behind `#[cfg(unix)]`. Windows uses `tokio::signal::ctrl_c()`
  inside the same `tokio::select!` loop and polls console size changes.
- **Tty detection**: `!args.dump` on Windows (no `libc::isatty`).
- **Pidfile management**: streamer and local-pane identity files plus exclusive
  pending launch claims work on both platforms. Windows records PID and process
  creation time, so PID reuse fails closed.
- **Docker container transport**: `resolve_blocking` gated behind `#[cfg(unix)]`;
  returns an explicit error on Windows.
- **`Msg::Resize` and `Msg::Hint`**: Resize is shared by Unix signals and the
  Windows polling path; hints remain Unix signal-driven.

Acceptance evidence:

- `cargo clippy --all-targets --locked -- -D warnings` — clean on Windows.
- `cargo test --locked` — 192 passed, 0 failed (including 7 Windows-specific tests
  for `term_size`, `isolate_session_process_group`, `attach_and_kill_process_group`,
  `attach_process_job` error path, `stdout_is_tty`, `RawMode` round-trip).
- `cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings` —
  clean (cross-target check).
- `cargo build --release --locked` — produces `target/release/herdr-mirror.exe`.

#### Phase 2B: Windows Streamer Lifecycle (attended path implemented; live gate pending)

- The attended Windows pane path uses identity files and Windows Job Objects for
  transport cleanup. Automatic daemon streamer launch and zombie healing remain
  fenced until the complete interactive lifecycle is validated.
- The attended pane polls console size and restores console modes on Ctrl-C and
  normal shutdown.

Remaining runtime blocker (not a compilation blocker):

- A live Windows-to-Linux mirror still needs manual validation for resize, Ctrl-C,
  reconnect, and clean shutdown. Output and input were exercised with a dedicated
  headless pane; the current executor cannot provide a Windows TTY for the
  remaining console-specific checks.
- The `pane_windows.rs` stub is retained but unused.

### Stage 3: Interactive Picker and File UX

Pending. Port the picker after ConPTY exists, reusing the same console layer.
Then validate Windows drag/drop path parsing and clipboard behavior separately.

Acceptance: host picker creation and Windows paths work without changing the
remote shell contract.

### Stage 4: Release and Operational Validation

Pending until Stages 2 and 3 are accepted.

- Publish signed/checksummed Windows assets alongside Unix assets.
- Validate `herdr plugin install` and `herdr plugin link` from Git Bash.
- Test upgrade, stale daemon recovery, pause/resume, and named-pipe cleanup.
- Remove the staged-support limitation only after the data-plane evidence exists.

## Boundaries and known gaps

- `herdr` itself is not modified by this port.
- Remote Linux/macOS sockets remain remote; only the local transport changes.
- `api_transport = "socket"` is rejected on Windows.
- The plugin manifest still invokes `./target/release/herdr-mirror` while the
  Windows asset is `herdr-mirror.exe`; confirm herdr appends `.exe` before
  relying on install-from-release, otherwise switch the manifest to the `.exe`
  name on Windows.
- `ssh` timeout on Windows kills only the direct child, not `ProxyCommand`
  descendants; Stage 2 owns the job-object fix.
- A Windows build passing Stage 1 must not be described as full pane support.
