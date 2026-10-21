// herdr-mirror daemon: lifecycle + sync loop (control plane).
//
//   herdr-mirror daemon       # foreground loop (what `start` spawns)
//   herdr-mirror start        # spawn detached daemon, write pidfile
//   herdr-mirror pause        # halt syncing (sticky); mirrors stay, resume with start
//   herdr-mirror ensure       # start only if not running (cheap event hook)
//   herdr-mirror status       # print daemon/host/mirror state
//   herdr-mirror once         # single converge pass, no daemon
//   herdr-mirror restore [host] [remote-id]   # un-tombstone closed mirrors
//   herdr-mirror teardown     # close all mirror workspaces, wipe id maps
//
// Each host runs as one task owning all its state: events, pokes, and timers
// arrive through one select loop, so converge and the status fast-path never
// interleave.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::api::{ApiClient, EventStream};
use crate::config::{load_config, HostConfig};
use crate::mirror::{
    apply_remote_closes, bump_local_epoch, converge, fetch_agents, mark_unknown, mirror_source,
    new_reports, push_statuses, regroup_sidebar, relay_pane_title, teardown, ConvergeDeps,
};
use crate::state::{load_state, save_state};
use crate::util::{err, now_iso, pid_alive, sleep_until_earliest, Env, Logger, Result};

// --- pidfile / pause marker ---

fn pid_path(env: &Env) -> PathBuf {
    env.state_dir.join("daemon.pid")
}

fn daemon_lock_path(env: &Env) -> PathBuf {
    env.state_dir.join("daemon.lock")
}

fn open_daemon_lock(env: &Env) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(daemon_lock_path(env))?)
}

fn try_lock(file: &fs::File) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(e.into())
    }
}

enum DaemonLockState {
    Unlocked,
    Locked(Option<i32>),
}

/// The lifetime lock is authoritative for daemons from this release onward.
/// Its contents identify the owner so commands can still signal the right
/// process if daemon.pid is deleted. An unlocked file may contain a stale PID;
/// the lock state, rather than the file's contents, decides whether it is live.
fn daemon_lock_state(env: &Env) -> Result<DaemonLockState> {
    let mut file = open_daemon_lock(env)?;
    if try_lock(&file)? {
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return Ok(DaemonLockState::Unlocked);
    }
    let mut contents = String::new();
    file.rewind()?;
    file.read_to_string(&mut contents)?;
    let pid = contents.trim().parse().ok().filter(|pid| pid_alive(*pid));
    Ok(DaemonLockState::Locked(pid))
}

struct DaemonLifetime {
    _lock: fs::File,
    pid_file: PathBuf,
    pid: i32,
}

impl Drop for DaemonLifetime {
    fn drop(&mut self) {
        // Never erase a newer owner's record if shutdowns overlap.
        let ours = fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            == Some(self.pid);
        if ours {
            let _ = fs::remove_file(&self.pid_file);
        }
        let _ = unsafe { libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn acquire_daemon_lifetime(env: &Env) -> Result<DaemonLifetime> {
    let mut lock = open_daemon_lock(env)?;
    if !try_lock(&lock)? {
        return Err(err("mirror daemon already running for this state directory"));
    }
    let pid = std::process::id() as i32;
    lock.set_len(0)?;
    lock.rewind()?;
    writeln!(lock, "{pid}")?;
    lock.flush()?;
    let guard = DaemonLifetime { _lock: lock, pid_file: pid_path(env), pid };
    fs::write(&guard.pid_file, pid.to_string())?;
    Ok(guard)
}

fn legacy_running_pid(env: &Env) -> Option<i32> {
    let pid: i32 = fs::read_to_string(pid_path(env)).ok()?.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
}

pub fn running_pid(env: &Env) -> Option<i32> {
    match daemon_lock_state(env) {
        Ok(DaemonLockState::Locked(pid)) => pid,
        // Compatibility with a daemon started by an older binary, which wrote
        // daemon.pid but did not hold daemon.lock for its lifetime.
        Ok(DaemonLockState::Unlocked) | Err(_) => legacy_running_pid(env),
    }
}

// Sticky pause marker: blocks the focus-hook autostart until an explicit
// start clears it (a crash leaves no marker, so it still auto-recovers).
fn pause_path(env: &Env) -> PathBuf {
    env.state_dir.join("daemon.paused")
}

pub fn is_paused(env: &Env) -> bool {
    pause_path(env).exists()
}

pub fn set_paused(env: &Env, paused: bool) {
    if paused {
        let _ = fs::write(pause_path(env), now_iso());
    } else {
        let _ = fs::remove_file(pause_path(env));
    }
}

// --- per-host runtime ---

struct HostCtx {
    env_state_dir: PathBuf,
    host: HostConfig,
    local: ApiClient,
    log: Logger,
    close_remote_on_local_close: bool,
    closes: crate::closes::Closes,
    /// this host's published journal copies; outlives every connection so a
    /// reconnect's first status push already carries them
    journals: crate::journal::Published,
}

/// Aborts the task when dropped: the journal task lives exactly as long as
/// the connection whose ControlMaster it shares.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Remote events, all broadcast (no `pane_id`). Every one of these is served
/// from herdr's in-memory event ring: subscribing costs the remote nothing per
/// pane.
const BROADCAST_SUBS: &[&str] = &[
    "workspace.created",
    "workspace.renamed",
    "workspace.closed",
    "tab.created",
    "tab.renamed",
    "tab.closed",
    "pane.created",
    "pane.closed",
    "pane.exited",
    // an agent appearing or leaving: answered with an immediate status poll,
    // not a converge — nothing structural changed
    "pane.agent_detected",
    // a bare remote resize (no pane created/closed) has no other event to
    // hang a converge off of
    "layout.updated",
];

/// Live title changes. `pane.updated` carries the whole pane on every change,
/// which for a busy agent means every spinner frame of its title — about five a
/// second (measured on herdr 0.9.0) — so these are coalesced and relayed, never
/// converged on.
const TITLE_SUB: &str = "pane.updated";
const TITLE_COALESCE: Duration = Duration::from_secs(1);

/// Agent status is polled with one `agent.list` per host, not subscribed per
/// pane. herdr 0.9.0 has no broadcast form of `pane.agent_status_changed` (the
/// subscription requires a `pane_id`), and it serves each such subscription by
/// polling: every 100 ms in which no event arrived, the subscription thread
/// makes a blocking `pane.get` round trip into the server's single app thread
/// (`src/api/subscriptions.rs`, `ActiveAgentStatusChangedSubscription::
/// poll_result`; `src/api/server.rs` `CONNECTION_POLL_INTERVAL`). Mirroring N
/// panes therefore cost every remote N x 10 app-thread requests a second, idle
/// or not. This is one request per host per interval.
const STATUS_POLL: Duration = Duration::from_secs(2);
/// Delay before the status poll an agent-detection event asks for, so a burst
/// (an agent's launcher, then the agent) is one poll.
const STATUS_NUDGE: Duration = Duration::from_millis(150);

/// Debounce for a converge after a structural remote event.
const CONVERGE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Longer for a bare remote resize: dragging a split emits a stream of these,
/// and only where it stopped matters.
const LAYOUT_DEBOUNCE: Duration = Duration::from_millis(1500);

fn sub_list(titles: bool) -> Vec<Value> {
    let mut subs: Vec<Value> = BROADCAST_SUBS.iter().map(|t| json!({ "type": t })).collect();
    if titles {
        subs.push(json!({ "type": TITLE_SUB }));
    }
    subs
}

/// herdr rejects a subscribe naming an event it doesn't know — the whole
/// request, by variant name. Matched on that exact variant: a newer server's
/// rejection of anything else also *lists* `pane.updated` among the expected.
fn rejects_title_events(err: &str) -> bool {
    err.contains(&format!("unknown variant `{TITLE_SUB}`"))
}

/// Subscribe with live title events while the remote accepts them. An older
/// remote gets the same subscription without them, and its mirrors' titles
/// follow each converge, instead of losing every other event along with them.
async fn subscribe(ctx: &HostCtx, remote: &ApiClient, titles: &mut bool) -> Result<EventStream> {
    if *titles {
        match remote.subscribe(sub_list(true)).await {
            Ok(s) => return Ok(s),
            Err(e) if rejects_title_events(&e.to_string()) => {
                *titles = false;
                ctx.log.log(&format!(
                    "[{}] remote has no {TITLE_SUB} events — mirror titles follow converge",
                    ctx.host.name
                ));
            }
            Err(e) => return Err(e),
        }
    }
    remote.subscribe(sub_list(false)).await
}

/// Earliest of `current` and `at`: a sooner request wins, a later one never
/// postpones what is already scheduled.
fn schedule(current: &mut Option<Instant>, at: Instant) {
    if current.is_none_or(|t| at < t) {
        *current = Some(at);
    }
}

/// Status poll: one `agent.list`, pushed through the change-detecting reporter
/// so only mirrors whose remote agent actually changed cost the hub anything.
/// The same list tells the journal task which panes to keep copies for.
async fn poll_status(
    ctx: &HostCtx,
    deps: &ConvergeDeps,
    journal: Option<&watch::Sender<Vec<crate::journal::Want>>>,
) -> Result<()> {
    // a hidden host's mirrors are gone locally; keep it quiet
    if crate::state::is_hidden(&ctx.env_state_dir, &ctx.host.name) {
        return Ok(());
    }
    let agents = fetch_agents(&deps.remote).await?;
    let mut state = load_state(&ctx.env_state_dir, &ctx.host.name);
    if let Some(tx) = journal {
        // every poll, changed or not: a working agent's journal is followed
        // on this beat
        let _ = tx.send(crate::journal::wants(&agents, &state));
    }
    let sent = push_statuses(
        &ctx.local,
        &deps.reports,
        &ctx.host.name,
        &ctx.log,
        &agents,
        deps.journals.as_ref(),
        &mut state,
    )
    .await;
    // titles ride along for agent panes (the poll already has them); a remote
    // without `pane.updated` gets its agent titles every poll this way
    for a in &agents {
        if let Some(entry) = state.panes.get(&a.pane_id) {
            relay_pane_title(&ctx.env_state_dir, &ctx.host.name, &a.pane_id, entry, a.terminal_title.as_deref());
        }
    }
    if sent {
        if let Err(e) = save_state(&ctx.env_state_dir, &ctx.host.name, &state) {
            ctx.log.log(&format!("[{}] state save failed: {e}", ctx.host.name));
        }
    }
    Ok(())
}

/// Title fast-path: relay coalesced `pane.updated` titles without a snapshot.
/// A pane the map doesn't know yet is skipped, not converged on: the converge
/// that creates its mirror relays its title anyway.
fn flush_titles(ctx: &HostCtx, pending: HashMap<String, Option<String>>) {
    // a hidden host's mirrors are gone locally; keep it quiet
    if crate::state::is_hidden(&ctx.env_state_dir, &ctx.host.name) {
        return;
    }
    let state = load_state(&ctx.env_state_dir, &ctx.host.name);
    for (remote_id, title) in pending {
        if let Some(entry) = state.panes.get(&remote_id) {
            relay_pane_title(&ctx.env_state_dir, &ctx.host.name, &remote_id, entry, title.as_deref());
        }
    }
}

/// Which transport the next reconnect should try first.
///
/// A fallback to the exec relay is only remembered once it has happened twice
/// running. The probe fails for transient reasons too (the remote herdr
/// restarting, a mux hiccup, one slow ping), and remembering the first one
/// pins a healthy host to the slower transport for the daemon's whole life
/// with a single log line as the only clue. A genuinely broken host wastes one
/// probe on the next reconnect and then sticks, which is what the memory is
/// for.
fn remember_transport(
    last: Option<crate::config::ApiTransport>,
    exec_streak: &mut u32,
) -> Option<crate::config::ApiTransport> {
    match last {
        Some(crate::config::ApiTransport::Exec) => {
            *exec_streak += 1;
            (*exec_streak >= 2).then_some(crate::config::ApiTransport::Exec)
        }
        other => {
            *exec_streak = 0;
            other
        }
    }
}

/// How far one dial got before it failed: the daemon's retry pacing and its
/// once-per-outage bookkeeping both depend on it.
#[derive(Default)]
struct Attempt {
    /// the remote's own `herdr status` answered (see `RemoteHost::reached`)
    reached: bool,
    /// the remote api answered, so this failure ended a live connection
    connected: bool,
}

/// Connected phase: subscribe, converge, then react to events/pokes/timers
/// until the connection drops (returns Err). `attempt` records how far the
/// dial got, so the caller can tell an unreachable host from a herdr that is
/// not serving, and a failed redial from the end of a live connection.
async fn run_connected(
    ctx: &HostCtx,
    poke: &mut mpsc::Receiver<()>,
    retry: &mut Retry,
    remembered_transport: &mut Option<crate::config::ApiTransport>,
    exec_streak: &mut u32,
    attempt: &mut Attempt,
) -> Result<()> {
    let mut remote_host = crate::remote::RemoteHost::new(&ctx.host, &ctx.env_state_dir);
    // a fresh RemoteHost is built on every reconnect, so what worked last
    // time (specifically: an auto host that fell back to the exec relay)
    // would otherwise be re-probed via streamlocal on every single reconnect
    // for the life of the daemon
    remote_host.hint_transport(*remembered_transport);
    let dialled = remote_host.connect_api().await;
    attempt.reached = remote_host.reached;
    let (remote, _status) = dialled?;
    *remembered_transport = remember_transport(remote_host.last_api_transport, exec_streak);
    retry.connected();
    attempt.connected = true;
    let deps = ConvergeDeps {
        local: ctx.local.clone(),
        remote: remote.clone(),
        host: ctx.host.clone(),
        state_dir: ctx.env_state_dir.clone(),
        log: ctx.log.clone(),
        close_remote_on_local_close: ctx.close_remote_on_local_close,
        closes: ctx.closes.clone(),
        // fresh per connection: `mark_unknown` overwrote everything on the way
        // down, so the first pass after a reconnect must send it all again
        reports: new_reports(),
        journals: Some(ctx.journals.clone()),
    };
    // Journal tails share this connection's transport and die with it. Opening
    // them publishes the copies already on disk before the first converge
    // reports statuses.
    let journal = (ctx.host.journal_tail_bytes > 0).then(|| {
        let (tx, rx) = watch::channel(Vec::new());
        let journals = crate::journal::Journals::open(
            &ctx.host.name,
            &ctx.env_state_dir,
            ctx.host.journal_tail_bytes,
            remote_host.executor(),
            ctx.journals.clone(),
            ctx.log.clone(),
        );
        (tx, AbortOnDrop(tokio::spawn(crate::journal::run(journals, rx))))
    });
    // whether this remote publishes pane.updated; settled by the first subscribe
    let mut title_events = true;
    let mut stream = subscribe(ctx, &remote, &mut title_events).await?;
    converge(&deps).await?;
    ctx.log.log(&format!("[{}] connected and synced", ctx.host.name));
    retry.synced();

    let mut converge_at: Option<Instant> = None;
    let mut status_at: Option<Instant> = Some(Instant::now() + STATUS_POLL);
    let mut closes_at: Option<Instant> = None;
    let mut titles_at: Option<Instant> = None;
    let mut pending_closes: Vec<String> = Vec::new();
    let mut pending_titles: HashMap<String, Option<String>> = HashMap::new();
    let mut status_failing = false;

    loop {
        let sleep = sleep_until_earliest([converge_at, status_at, closes_at, titles_at]);
        tokio::select! {
            ev = stream.next() => {
                let now = Instant::now();
                match ev {
                    None => return Err(err("event stream closed")),
                    Some(e) if e.event == "pane_agent_detected" => {
                        schedule(&mut status_at, now + STATUS_NUDGE);
                    }
                    // explicit remote closes are authoritative: remove the mirror
                    // directly instead of inferring it from snapshot absence
                    Some(e) if matches!(e.event.as_str(), "workspace_closed" | "tab_closed" | "pane_closed") => {
                        let key = match e.event.as_str() {
                            "workspace_closed" => "workspace_id",
                            "tab_closed" => "tab_id",
                            _ => "pane_id",
                        };
                        if let Some(rid) = e.data.get(key).and_then(|v| v.as_str()) {
                            pending_closes.push(rid.to_string());
                            closes_at.get_or_insert(now + Duration::from_millis(150));
                        }
                    }
                    // title changes: coalesced per pane and relayed to the
                    // streamers. Never the generic converge below — a busy
                    // agent's spinner alone would converge five times a second
                    Some(e) if e.event == "pane_updated" => {
                        let pane = e.data.get("pane");
                        if let Some(pid) = pane.and_then(|p| p.get("pane_id")).and_then(Value::as_str) {
                            let title = pane
                                .and_then(|p| p.get("terminal_title"))
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            pending_titles.insert(pid.to_string(), title);
                            titles_at.get_or_insert(now + TITLE_COALESCE);
                        }
                    }
                    Some(e) if e.event == "layout_updated" => {
                        schedule(&mut converge_at, now + LAYOUT_DEBOUNCE);
                    }
                    Some(_) => {
                        schedule(&mut converge_at, now + CONVERGE_DEBOUNCE);
                    }
                }
            }
            Some(()) = poke.recv() => {
                converge_at = Some(Instant::now());
            }
            _ = sleep => {
                let now = Instant::now();
                if closes_at.is_some_and(|t| t <= now) {
                    closes_at = None;
                    let closed = std::mem::take(&mut pending_closes);
                    apply_remote_closes(&ctx.local, &ctx.env_state_dir, &ctx.host.name, &closed, &ctx.log).await;
                    // reconcile after the removals
                    converge_at = Some(now);
                }
                if titles_at.is_some_and(|t| t <= now) {
                    titles_at = None;
                    flush_titles(ctx, std::mem::take(&mut pending_titles));
                }
                if converge_at.is_some_and(|t| t <= now) {
                    converge_at = None;
                    // a hide pressed while we are up arrives as a poke, and
                    // converge only freezes — the closing is here
                    crate::mirror::apply_hidden(
                        &ctx.local,
                        &ctx.env_state_dir,
                        &ctx.host.name,
                        &ctx.log,
                        &ctx.closes,
                    )
                    .await;
                    converge(&deps).await?;
                }
                if status_at.is_some_and(|t| t <= now) {
                    match poll_status(ctx, &deps, journal.as_ref().map(|(tx, _)| tx)).await {
                        Ok(()) => status_failing = false,
                        // the event stream is what decides the connection is
                        // gone; one slow answer is not worth a reconnect
                        Err(e) => {
                            if !status_failing {
                                ctx.log.log(&format!("[{}] status poll failed: {e}", ctx.host.name));
                            }
                            status_failing = true;
                        }
                    }
                    status_at = Some(Instant::now() + STATUS_POLL);
                }
            }
        }
    }
}

/// Retry pacing after a lost connection.
///
/// An unreachable machine backs off exponentially from 5 s to 5 min: the first
/// retries come quickly (a blip, a reboot), and a machine that is simply off
/// costs one dial every 5 min instead of two ssh spawns every 31 s forever.
///
/// A machine that answers while its herdr does not — the server restarting,
/// shutting down, or stopped — is redialled every 2 s for up to 3 min. A
/// restart is over in seconds, and the ladder would leave the mirror (and
/// every agent status on it) waiting minutes for a server long since back;
/// each of those redials is one `herdr status` over the live ssh master.
/// A herdr still down after the window joins the ladder.
///
/// A stopped container is not a fault, it is the resting state, and waits the
/// dormant interval from the start.
const RECONNECT_FIRST: u64 = 5;
const RECONNECT_MAX: u64 = 300;
const DORMANT_DELAY: u64 = 300;
const HERDR_DOWN_RETRY: u64 = 2;
const HERDR_DOWN_WINDOW: Duration = Duration::from_secs(180);

fn reconnect_delay(attempt: usize) -> u64 {
    RECONNECT_FIRST
        .saturating_mul(1u64 << attempt.min(16))
        .min(RECONNECT_MAX)
}

/// Why a dial, or a live connection, failed — as far as pacing cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// ssh (or docker) never got an answer out of the machine: off, off the
    /// network, refusing us, or timing out
    Unreachable,
    /// the machine answered but its herdr is not serving: not running,
    /// shutting down, its api socket gone, or the event stream cut
    HerdrDown,
    /// a stopped container
    Dormant,
}

/// `reached` is `Attempt::reached`: the remote's own `herdr status` answered
/// (and said nothing fast retrying cannot fix), so everything that failed
/// after it — the server reported down, the api ping, a subscribe or converge
/// cut short — is herdr's, not the network's. Classifying by how far the dial
/// got, rather than by matching error text, covers every message herdr and
/// ssh may print without listing them.
///
/// The dormant marker is matched with starts_with, not contains: it is always
/// emitted as a prefix, while the error text can embed user strings (target,
/// remote_bin), and a substring test would make an ssh host named
/// `dormant-box` wait out the dormant interval on every genuine failure.
fn classify(error: &str, reached: bool) -> Failure {
    if error.starts_with(crate::remote::DORMANT) {
        Failure::Dormant
    } else if reached {
        Failure::HerdrDown
    } else {
        Failure::Unreachable
    }
}

/// What a retry's log line should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// first failure of a herdr-down run: announce the fast retries
    FastStart,
    /// inside the fast window: quiet, its start was logged
    Fast,
    /// the fast window ran out with herdr still down: say so once
    FastSpent,
    /// the ladder or the dormant wait: logged per `retry_log_due`
    Slow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// no herdr-down run in progress
    Closed,
    /// fast retrying since this instant
    Open(Instant),
    /// fast retrying ran out; herdr-down failures climb the ladder
    Spent,
}

/// One host's retry pacing across reconnects.
struct Retry {
    /// next rung of the unreachable ladder
    rung: usize,
    window: Window,
}

impl Retry {
    fn new() -> Retry {
        Retry { rung: 0, window: Window::Closed }
    }

    /// The remote api answered: the ladder starts over.
    fn connected(&mut self) {
        self.rung = 0;
    }

    /// Connected and synced: the next herdr outage gets a fresh fast window.
    /// Not at `connected` — a connection that fails its first sync every time
    /// would reopen the window forever and redial every 2 s for good.
    fn synced(&mut self) {
        self.window = Window::Closed;
    }

    /// Seconds until the next dial after `failure` at `now`.
    fn next(&mut self, failure: Failure, now: Instant) -> (u64, Pace) {
        match failure {
            // dormant cycles must not advance the ladder they do not use: a
            // container stopped overnight would otherwise leave the next real
            // failure while it boots waiting 5 minutes instead of 5 seconds
            Failure::Dormant => {
                self.window = Window::Closed;
                (DORMANT_DELAY, Pace::Slow)
            }
            // a host that went unreachable mid-run and came back with herdr
            // still starting (a reboot) earns a fresh window
            Failure::Unreachable => {
                self.window = Window::Closed;
                (self.climb(), Pace::Slow)
            }
            Failure::HerdrDown => match self.window {
                Window::Closed => {
                    self.window = Window::Open(now);
                    (HERDR_DOWN_RETRY, Pace::FastStart)
                }
                Window::Open(since) if now.duration_since(since) < HERDR_DOWN_WINDOW => {
                    (HERDR_DOWN_RETRY, Pace::Fast)
                }
                Window::Open(_) => {
                    self.window = Window::Spent;
                    (self.climb(), Pace::FastSpent)
                }
                Window::Spent => (self.climb(), Pace::Slow),
            },
        }
    }

    fn climb(&mut self) -> u64 {
        let delay = reconnect_delay(self.rung);
        self.rung += 1;
        delay
    }
}

/// A repeat of the error last logged stays quiet for this long.
const RETRY_LOG_QUIET: Duration = Duration::from_secs(3600);

/// Is this failed dial worth a log line? The first of a streak, an error that
/// differs from the one last logged, or the same one an hour on — so a host
/// that is off is one line an hour, not one per retry.
fn retry_log_due(last: Option<&(String, Instant)>, error: &str, now: Instant) -> bool {
    match last {
        None => true,
        Some((logged, at)) => logged != error || now.duration_since(*at) >= RETRY_LOG_QUIET,
    }
}

async fn host_task(ctx: HostCtx, mut poke: mpsc::Receiver<()>) {
    let mut retry = Retry::new();
    // persists across reconnects for the daemon's whole lifetime — the
    // point of remembering at all (see `run_connected`)
    let mut remembered_transport: Option<crate::config::ApiTransport> = None;
    let mut exec_streak = 0u32;
    // the error last logged for this host's failure streak, and when
    let mut last_logged: Option<(String, Instant)> = None;
    let mut failures = 0u64;
    loop {
        // Before dialling, and again after every failed dial: taking a hidden
        // host's mirrors down needs only the LOCAL api, so it must not wait on a
        // remote that may never come back. That case — a dead host leaving
        // reconnecting panes on screen — is the main reason to hide one.
        crate::mirror::apply_hidden(
            &ctx.local,
            &ctx.env_state_dir,
            &ctx.host.name,
            &ctx.log,
            &ctx.closes,
        )
        .await;
        let mut attempt = Attempt::default();
        let e = match run_connected(
            &ctx,
            &mut poke,
            &mut retry,
            &mut remembered_transport,
            &mut exec_streak,
            &mut attempt,
        )
        .await
        {
            Ok(()) => unreachable!("run_connected only returns on error"),
            Err(e) => e,
        };
        if attempt.connected {
            last_logged = None;
            failures = 0;
        }
        // Once per outage: when a live connection ends, and on the first failed
        // dial after startup (a previous daemon may have left these mirrors
        // claiming a state nobody is updating). Every failed redial after that
        // would only re-send the same "unknown".
        if failures == 0 {
            mark_unknown(&ctx.local, &ctx.env_state_dir, &ctx.host.name, "mirror: connection lost").await;
        }
        failures += 1;
        let text = e.to_string();
        let now = Instant::now();
        let (delay, pace) = retry.next(classify(&text, attempt.reached), now);
        let streak = if failures > 1 { format!(", attempt {failures}") } else { String::new() };
        match pace {
            Pace::Fast => {}
            Pace::FastStart => {
                ctx.log.log(&format!(
                    "[{}] disconnected ({text}) — retrying every {HERDR_DOWN_RETRY}s for up to {}s while the host answers{streak}",
                    ctx.host.name,
                    HERDR_DOWN_WINDOW.as_secs()
                ));
                // whatever ends the fast run is news
                last_logged = None;
            }
            Pace::FastSpent | Pace::Slow => {
                if pace == Pace::FastSpent || retry_log_due(last_logged.as_ref(), &text, now) {
                    ctx.log.log(&format!(
                        "[{}] disconnected ({text}) — retrying in {delay}s{streak}; repeats of this error are logged hourly",
                        ctx.host.name
                    ));
                    last_logged = Some((text, now));
                }
            }
        }
        // drain FIRST: pokes that piled up during a multi-second dial say nothing
        // about now, and honouring them would skip the sleep entirely
        while poke.try_recv().is_ok() {}
        // Wake early only for a hidden host, whose close is genuinely waiting on
        // us and would otherwise sit behind a long sleep. Every other poke is
        // ordinary local traffic, and treating them as urgent collapses the
        // backoff and burns a dial (or a `docker ps`) per poke.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(delay);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = poke.recv() => {
                    if crate::state::is_hidden(&ctx.env_state_dir, &ctx.host.name) {
                        break;
                    }
                }
            }
        }
        while poke.try_recv().is_ok() {}
    }
}

/// Does this local pane already have a live streamer?
///
/// Asks herdr what is actually running in the pane, rather than inferring it
/// from a global `ps` scan and string-matching argv. That inference is what
/// previously required a host-identity token in the argv, end-of-argument
/// anchoring so `work` could not match `work-staging`, and a compatibility
/// shim for streamers predating that token. None of it is needed to answer the
/// only question that matters: is something already running in THIS pane.
///
/// Generation-agnostic by construction — every streamer ever shipped is
/// `herdr-mirror pane …`, whatever flags follow.
async fn has_live_streamer(local: &ApiClient, pane_id: &str) -> Option<bool> {
    let v = local.request("pane.process_info", json!({ "pane_id": pane_id })).await.ok()?;
    let procs = v.pointer("/process_info/foreground_processes")?.as_array()?;
    Some(procs.iter().any(|p| {
        p.get("argv").and_then(|a| a.as_array()).is_some_and(|argv| is_streamer_argv(argv))
    }))
}

fn streamer_recovery_needed(process_info_live: Option<bool>, pidfile_live: bool) -> bool {
    process_info_live == Some(false) && !pidfile_live
}

/// Is this foreground process one of our pane wrappers?
///
/// argv[0] is the resolved exe path, which varies by install (release build,
/// plugin checkout, `cargo run`), so it is matched by suffix. argv[1] pins the
/// subcommand so an unrelated `herdr-mirror status` in the pane is not mistaken
/// for a live stream.
fn is_streamer_argv(argv: &[Value]) -> bool {
    argv.first().and_then(|s| s.as_str()).is_some_and(|e| e.ends_with("herdr-mirror"))
        && argv.get(1).and_then(|s| s.as_str()) == Some("pane")
}

/// After a local herdr server restart, session-restore resurrects mirror panes
/// as plain shells: their ids match the map, but no streamer processes exist —
/// and converge can't tell (the snapshot has no process info), so the mirrors
/// sit frozen forever. Heal = re-exec the streamer into each pane that is not
/// already running one. A transient socket blip leaves wrappers running, so
/// the check stays quiet then.
async fn heal_zombie_mirrors(
    local: &ApiClient,
    state_dir: &std::path::Path,
    hosts: &[HostConfig],
    pokers: &[mpsc::Sender<()>],
    log: &Logger,
) {
    for (i, h) in hosts.iter().enumerate() {
        let state = load_state(state_dir, &h.name);
        let panes: Vec<(String, String)> = state
            .panes
            .iter()
            .filter(|(_, e)| !e.is_tombstoned())
            .map(|(rid, e)| (rid.clone(), e.local_id.clone()))
            .collect();
        if panes.is_empty() {
            continue;
        }
        // Ask per pane, so one live streamer no longer blocks healing every
        // other dead pane on the same host.
        //
        // Fail SAFE: anything other than a definite "nothing running there" is
        // treated as alive. Leaving a frozen mirror is recoverable and visible;
        // exec'ing into a pane whose streamer owns stdin writes the command
        // line into the user's live remote session instead.
        let mut dead: Vec<(String, String)> = Vec::new();
        for (remote_pane_id, local_pane_id) in panes {
            let process_info_live = has_live_streamer(local, &local_pane_id).await;
            let pidfile_live = crate::util::streamer_alive(state_dir, &h.target, &remote_pane_id)
                || crate::util::pane_streamer_alive(state_dir, &local_pane_id);
            if streamer_recovery_needed(process_info_live, pidfile_live) {
                dead.push((remote_pane_id, local_pane_id));
            }
        }
        if dead.is_empty() {
            continue;
        }
        log.log(&format!(
            "[{}] {} mirror pane(s) mapped but not streaming (server restart?) — re-exec'ing streamers",
            h.name,
            dead.len()
        ));
        // Surgical on purpose: session-restore brought the workspace, tabs, panes
        // and layout back intact — only the streamer processes died. Exec the
        // streamer back into each existing pane rather than closing the workspace
        // and rebuilding it: that rebuild raced its own close (the fresh snapshot
        // still listed the dying workspace, so the adopt path reused it and
        // layout.apply then failed on its dead tab).
        //
        // Sizes live in the remote layout, which we don't have here; the wrapper
        // falls back to its default and the next converge reconciles.
        let cmd_for = crate::mirror::cmd_for_pane(h, state_dir, &HashMap::new());
        for (remote_pane_id, local_pane_id) in dead {
            let argv = cmd_for(&remote_pane_id);
            crate::mirror::spawn_streamer_pane(local, state_dir, &local_pane_id, &argv, log).await;
        }
        let _ = pokers[i].try_send(());
    }
}

/// The local object a hub event is about, when it can concern a mirror: the
/// id whose owning host must reconcile. `None` for events no host acts on.
fn local_event_target(event: &str, data: &Value) -> Option<String> {
    let id = match event {
        "workspace_closed" => data.get("workspace_id"),
        "pane_closed" => data.get("pane_id"),
        "tab_closed" | "tab_renamed" => data.get("tab_id"),
        // `layout.updated` carries the whole layout of the tab that changed
        "layout_updated" => data.pointer("/layout/tab_id").or_else(|| data.get("tab_id")),
        _ => None,
    };
    id.and_then(Value::as_str).map(str::to_string)
}

/// Indices of the hosts whose id map holds `local_id`. herdr ids are unique per
/// server, so this is at most one host in practice; an unmapped id (a local,
/// non-mirror object) is none.
fn owning_hosts(state_dir: &std::path::Path, hosts: &[HostConfig], local_id: &str) -> Vec<usize> {
    hosts
        .iter()
        .enumerate()
        .filter(|(_, h)| {
            let s = load_state(state_dir, &h.name);
            s.workspaces.values().any(|e| e.local_id == local_id)
                || s.tabs.values().any(|e| e.local_id == local_id)
                || s.panes.values().any(|e| e.local_id == local_id)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Local events: a close, rename or resize of a mirror pokes the host that
/// owns it, so its next converge records the user's intent promptly. Only that
/// host: the operator's own resizes and closes elsewhere on the hub used to
/// make every host take two snapshots per event.
async fn local_events_task(
    local: ApiClient,
    pokers: Vec<mpsc::Sender<()>>,
    prefixes: Vec<String>,
    hosts: Vec<HostConfig>,
    state_dir: PathBuf,
    log: Logger,
    closes: crate::closes::Closes,
) {
    loop {
        let subs = vec![
            json!({ "type": "workspace.created" }),
            json!({ "type": "workspace.closed" }),
            json!({ "type": "pane.closed" }),
            // closing a TAB emits only tab_closed — no pane_closed for the
            // panes inside it — so without this a tab close never counts as
            // user intent and close-through silently degrades to tombstoning
            json!({ "type": "tab.closed" }),
            // renaming a mirror tab locally is intent for the remote tab, which
            // converge resolves against the label it last stamped; without this
            // the rename is never noticed and the next converge reverts it
            json!({ "type": "tab.renamed" }),
            // resizing a mirror pane locally is an edit the remote should
            // follow on a host we drive; the poke below is what gets it there
            // promptly instead of on the next unrelated event
            json!({ "type": "layout.updated" }),
        ];
        match local.subscribe(subs).await {
            Ok(mut stream) => {
                // the server may have restarted since anything was reported to
                // it: every host re-sends its reports on the next pass
                bump_local_epoch();
                // catch a sidebar left ungrouped from a previous run
                regroup_sidebar(&local, &prefixes, &log).await;
                // subscribe succeeding after a drop = the server is back up;
                // give session-restore a beat, then sweep for zombie mirrors
                tokio::time::sleep(Duration::from_secs(3)).await;
                heal_zombie_mirrors(&local, &state_dir, &hosts, &pokers, &log).await;
                while let Some(e) = stream.next().await {
                    // A close EVENT is the authoritative "the user closed this";
                    // snapshot absence is not (rebuild/restart/failed converge).
                    // Our own closes are marked beforehand and swallowed here.
                    let key = match e.event.as_str() {
                        "workspace_closed" => Some("workspace_id"),
                        "pane_closed" => Some("pane_id"),
                        "tab_closed" => Some("tab_id"),
                        _ => None,
                    };
                    if let Some(k) = key {
                        if let Some(lid) = e.data.get(k).and_then(|v| v.as_str()) {
                            if let Ok(mut t) = closes.lock() {
                                t.note_close_event(lid);
                            }
                        }
                    }
                    if let Some(id) = local_event_target(&e.event, &e.data) {
                        for i in owning_hosts(&state_dir, &hosts, &id) {
                            let _ = pokers[i].try_send(());
                        }
                    }
                    // only a workspace appearing or leaving can ungroup the
                    // sidebar (no-op if already grouped)
                    if matches!(e.event.as_str(), "workspace_created" | "workspace_closed") {
                        regroup_sidebar(&local, &prefixes, &log).await;
                    }
                }
                log.log("local event stream dropped — resubscribing");
            }
            Err(e) => log.log(&format!("local subscribe failed ({e}) — retrying")),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// --- commands ---

pub async fn cmd_run(env: Env) -> Result<()> {
    let _daemon_lifetime = acquire_daemon_lifetime(&env)?;
    let detached = std::env::var("HERDR_MIRROR_DETACHED").is_ok();
    let log = Logger::new(&env.state_dir, !detached);
    let config = load_config(&env.config_search)?;
    log.log(&format!(
        "daemon starting (pid {}, hosts: {}, config: {})",
        std::process::id(),
        config.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", "),
        config.source.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "?".into())
    ));
    // two configs on disk is a silent trap: the loser is ignored with no sign
    for ignored in &config.shadowed {
        log.log(&format!("warning: ignoring shadowed config at {}", ignored.display()));
    }
    // a skipped host would otherwise just be quietly absent from the sidebar
    for w in &config.warnings {
        log.log(&format!("warning: {w}"));
    }

    let local = ApiClient::connect(&env.local_socket).await?;
    // Detect-and-report only: a broken CLI link means every keybinding through
    // it dies silently, but the repair stays behind the explicit `start`
    // command — the daemon never rewrites the filesystem in the background.
    if let Some(problem) = crate::util::cli_link_problem() {
        log.log(&format!("warning: {problem} — keybindings using it can't fire; run start to repair"));
        let _ = local
            .request(
                "notification.show",
                json!({
                    "title": "mirror: CLI link broken",
                    "body": format!("{problem} — run Mirror: start (or herdr-mirror start) to repair"),
                }),
            )
            .await;
    }
    let closes = crate::closes::new_closes();
    // copies left by a host that is gone from the config (or has journal
    // tails off) have nobody to follow or delete them
    let journal_hosts: std::collections::HashSet<String> =
        config.hosts.iter().filter(|h| h.journal_tail_bytes > 0).map(|h| h.name.clone()).collect();
    crate::journal::prune_foreign(&env.state_dir, &journal_hosts, &log);
    if !journal_hosts.is_empty() {
        let (state_dir, log) = (env.state_dir.clone(), log.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(crate::journal::BLOB_GC_EVERY).await;
                crate::journal::gc_blobs(&state_dir, &log);
            }
        });
    }
    let mut pokers: Vec<mpsc::Sender<()>> = Vec::new();
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for h in &config.hosts {
        let (tx, rx) = mpsc::channel(8);
        pokers.push(tx);
        let ctx = HostCtx {
            env_state_dir: env.state_dir.clone(),
            host: h.clone(),
            local: local.clone(),
            log: log.clone(),
            close_remote_on_local_close: config.close_remote_on_local_close,
            closes: closes.clone(),
            journals: crate::journal::new_published(),
        };
        tasks.push(tokio::spawn(host_task(ctx, rx)));
    }
    let prefixes: Vec<String> = config.hosts.iter().map(|h| h.prefix.clone()).collect();
    tasks.push(tokio::spawn(local_events_task(
        local.clone(),
        pokers.clone(),
        prefixes,
        config.hosts.clone(),
        env.state_dir.clone(),
        log.clone(),
        closes.clone(),
    )));

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let mut poll = tokio::time::interval(Duration::from_secs(config.poll_seconds.max(5)));
    poll.tick().await; // consume the immediate first tick (initial sync already runs)

    loop {
        tokio::select! {
            _ = poll.tick() => {
                for p in &pokers {
                    let _ = p.try_send(());
                }
            }
            _ = sigusr1.recv() => {
                // restore pokes us instead of converging itself — single writer
                log.log("sync poke received");
                for p in &pokers {
                    let _ = p.try_send(());
                }
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    log.log("daemon stopping — clearing agent authority on mirror panes");
    // stop sync work first, or a live host task could re-report after the clear
    for t in &tasks {
        t.abort();
    }
    for h in &config.hosts {
        let state = load_state(&env.state_dir, &h.name);
        for entry in state.panes.values() {
            if entry.is_tombstoned() {
                continue;
            }
            let _ = local
                .request(
                    "pane.clear_agent_authority",
                    json!({ "pane_id": entry.local_id, "source": mirror_source(&h.name) }),
                )
                .await;
        }
    }
    Ok(())
}

pub fn cmd_start(env: &Env) -> Result<()> {
    // Serialize launch attempts. The child separately owns daemon.lock for its
    // full lifetime; this short lock only keeps two `start` calls from both
    // spawning candidates before either child publishes readiness.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(env.state_dir.join("daemon.start.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(err("cannot lock daemon.start.lock"));
    }
    if running_pid(env).is_some() {
        println!("mirror daemon already running");
        return Ok(());
    }
    let exe = crate::util::self_exe_path()
        .ok_or_else(|| err("cannot locate the herdr-mirror binary to respawn"))?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env.state_dir.join("daemon.log"))?;
    let log2 = log.try_clone()?;
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log2)
        .env("HERDR_MIRROR_DETACHED", "1")
        .process_group(0)
        .spawn()?;
    let expected = child.id() as i32;
    for _ in 0..100 {
        if running_pid(env) == Some(expected) {
            println!("mirror daemon started (pid {expected})");
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(err(format!("mirror daemon exited during startup ({status})")));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(err(format!("mirror daemon did not become ready (pid {expected})")))
}

pub fn cmd_pause(env: &Env) {
    // sticky: mirrors stay, only the sync loop halts; resume with start
    set_paused(env, true);
    match running_pid(env) {
        None => println!("mirror daemon already stopped; paused (won't autostart until you run start)"),
        Some(pid) => {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            println!("paused mirror daemon (pid {pid}); mirrors stay, resume with start");
        }
    }
}

pub fn cmd_ensure(env: &Env) {
    // focus-hook path: cheap, silent, honors autostart opt-out + sticky pause
    if running_pid(env).is_some() || is_paused(env) {
        return;
    }
    match load_config(&env.config_search) {
        Ok(c) if c.autostart => {
            let _ = cmd_start(env);
        }
        _ => { /* no/invalid config → nothing to start */ }
    }
}

pub fn cmd_status(env: &Env) -> Result<()> {
    match running_pid(env) {
        Some(pid) => println!("daemon: running (pid {pid})"),
        None => println!(
            "daemon: not running{}",
            if is_paused(env) { " (paused — resume with start)" } else { "" }
        ),
    }
    match crate::util::cli_link_state() {
        crate::util::CliLink::Ok(t) => println!("cli link: ok (-> {})", t.display()),
        crate::util::CliLink::Missing => {
            println!("cli link: MISSING ({}) — keybindings using it can't fire; start repairs it", crate::util::cli_link_path().display())
        }
        crate::util::CliLink::Dangling(t) => {
            println!("cli link: BROKEN (-> {}) — keybindings using it can't fire; start repairs it", t.display())
        }
        crate::util::CliLink::Other(t) => println!("cli link: -> {} (not this binary; left alone)", t.display()),
        crate::util::CliLink::File => {
            println!("cli link: {} is a regular file (not managed)", crate::util::cli_link_path().display())
        }
    }
    let config = load_config(&env.config_search)?;
    if let Some(src) = &config.source {
        println!("config: {}", src.display());
    }
    for ignored in &config.shadowed {
        println!("warning: ignoring shadowed config at {}", ignored.display());
    }
    for w in &config.warnings {
        println!("warning: {w}");
    }
    for h in &config.hosts {
        let state = load_state(&env.state_dir, &h.name);
        let ws = state.workspaces.values().filter(|w| !w.is_tombstoned()).count();
        let panes = state.panes.values().filter(|p| !p.is_tombstoned()).count();
        // `hidden` persists across reboots, so without this a host hidden weeks
        // ago is indistinguishable from one whose remote is unreachable: both
        // print zero mirrors and a healthy daemon
        let hidden = match crate::state::is_hidden(&env.state_dir, &h.name) {
            // marker set but the mirrors are still mapped: the daemon has not
            // applied it yet (it is stopped, or the host has not looped)
            true if ws > 0 || panes > 0 => {
                " — HIDDEN (pending; start the daemon to apply)".to_string()
            }
            true => format!(" — HIDDEN, `herdr-mirror show {}` brings it back", h.name),
            false => String::new(),
        };
        println!(
            "host {} ({}): {ws} mirror workspaces, {panes} mirror panes{hidden}",
            h.name, h.target
        );
        let tombs: Vec<String> = state
            .workspaces
            .iter()
            .filter(|(_, e)| e.is_tombstoned())
            .map(|(rid, _)| format!("workspace {rid}"))
            .chain(state.panes.iter().filter(|(_, e)| e.is_tombstoned()).map(|(rid, _)| format!("pane {rid}")))
            .collect();
        if !tombs.is_empty() {
            println!("  closed mirrors (restorable): {}", tombs.join(", "));
        }
    }
    let log_file = env.state_dir.join("daemon.log");
    if let Ok(text) = fs::read_to_string(&log_file) {
        println!("recent log:");
        for l in text.trim_end().lines().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
            println!("  {l}");
        }
    }
    Ok(())
}

pub async fn cmd_once(env: Env) -> Result<()> {
    let log = Logger::new(&env.state_dir, true);
    let config = load_config(&env.config_search)?;
    let local = ApiClient::connect(&env.local_socket).await?;
    for h in &config.hosts {
        // converge would only freeze anyway, and dialling a host someone hid
        // *because* it is dead would abort the whole run with `?`
        if crate::state::is_hidden(&env.state_dir, &h.name) {
            println!("{}: hidden — skipped", h.name);
            continue;
        }
        let mut remote_host = crate::remote::RemoteHost::new(h, &env.state_dir);
        let (remote, _status) = remote_host.connect_api().await?;
        converge(&ConvergeDeps {
            local: local.clone(),
            remote,
            host: h.clone(),
            state_dir: env.state_dir.clone(),
            log: log.clone(),
            close_remote_on_local_close: config.close_remote_on_local_close,
            // one-shot: no local event stream, so there is no authoritative
            // close signal — an empty tracker means this pass syncs but never
            // closes a remote object, which is the correct conservative default
            closes: crate::closes::new_closes(),
            // one pass: nothing was sent before it, so everything is sent
            reports: new_reports(),
            // the daemon owns the journal copies; leave their tokens alone
            journals: None,
        })
        .await?;
        log.log(&format!("[{}] one-shot mirror complete", h.name));
    }
    Ok(())
}

/// Un-tombstone mirrors the user closed: deleting the entries makes converge
/// recreate them through the normal paths. Pokes the daemon; never converges.
pub fn cmd_restore(env: &Env, filter_host: Option<&str>, filter_id: Option<&str>) -> Result<()> {
    let config = load_config(&env.config_search)?;
    let mut cleared = 0usize;
    for h in &config.hosts {
        if filter_host.is_some_and(|f| f != h.name) {
            continue;
        }
        let mut state = load_state(&env.state_dir, &h.name);
        let ws_doomed: Vec<String> = state
            .workspaces
            .iter()
            .filter(|(rid, e)| e.is_tombstoned() && filter_id.is_none_or(|f| f == rid.as_str()))
            .map(|(rid, _)| rid.clone())
            .collect();
        let pane_doomed: Vec<String> = state
            .panes
            .iter()
            .filter(|(rid, e)| e.is_tombstoned() && filter_id.is_none_or(|f| f == rid.as_str()))
            .map(|(rid, _)| rid.clone())
            .collect();
        for rid in &ws_doomed {
            state.workspaces.remove(rid);
        }
        for rid in &pane_doomed {
            state.panes.remove(rid);
        }
        cleared += ws_doomed.len() + pane_doomed.len();
        save_state(&env.state_dir, &h.name, &state)?;
    }
    if cleared == 0 {
        println!("nothing to restore (no tombstoned mirrors matched)");
        return Ok(());
    }
    // a hidden host freezes converge, so the tombstones really are gone but
    // nothing reappears until `show` — say so rather than claiming a sync
    let hidden: Vec<&str> = config
        .hosts
        .iter()
        .filter(|h| filter_host.is_none_or(|f| f == h.name))
        .filter(|h| crate::state::is_hidden(&env.state_dir, &h.name))
        .map(|h| h.name.as_str())
        .collect();
    if !hidden.is_empty() {
        println!(
            "note: {} is hidden — run `herdr-mirror show {}` to bring mirrors back",
            hidden.join(", "),
            hidden[0]
        );
    }
    match running_pid(env) {
        Some(pid) => {
            unsafe { libc::kill(pid, libc::SIGUSR1) };
            println!("restored {cleared} mirror(s) — daemon syncing now");
        }
        None => println!("restored {cleared} mirror(s) — they will reappear when the daemon starts"),
    }
    Ok(())
}

pub async fn cmd_teardown(env: Env) -> Result<()> {
    let log = Logger::new(&env.state_dir, true);
    if let Some(pid) = running_pid(&env) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    set_paused(&env, true); // torn down stays down until an explicit start
    let config = load_config(&env.config_search)?;
    let local = ApiClient::connect(&env.local_socket).await?;
    for h in &config.hosts {
        teardown(&local, &env.state_dir, &h.name, &log, None).await?;
    }
    log.log("teardown complete (autostart paused until next start)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detection is a separate lifecycle event from later status changes. If
    /// this broadcast subscription disappears, a newly detected agent can sit
    /// stale until the next status poll even though its pane is mirrored.
    ///
    /// And no subscription may name a pane: herdr 0.9.0 serves every
    /// `pane.agent_status_changed{pane_id}` by polling `pane.get` into its app
    /// thread ten times a second, which is the load this daemon must not put on
    /// the machines it mirrors. Status comes from the `agent.list` poll.
    #[test]
    fn subscriptions_are_broadcast_only() {
        for titles in [true, false] {
            let subs = sub_list(titles);
            assert!(subs.contains(&json!({ "type": "pane.agent_detected" })));
            for s in &subs {
                assert!(s.get("pane_id").is_none(), "per-pane subscription: {s}");
                assert_ne!(s["type"], "pane.agent_status_changed");
                assert_ne!(s["type"], "pane.output_matched");
                assert_ne!(s["type"], "pane.scroll_changed");
            }
        }
    }

    /// Unreachable hosts back off 5 s doubling to a 5 min ceiling, and stay
    /// there however long the outage lasts.
    #[test]
    fn unreachable_hosts_back_off_exponentially_to_five_minutes() {
        let ladder: Vec<u64> = (0..9).map(reconnect_delay).collect();
        assert_eq!(ladder, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
        assert_eq!(reconnect_delay(10_000), 300);
        // a day at the ceiling is 288 dials, where the old 30 s rung was 2880
        let day: u64 = (0..400).map(reconnect_delay).scan(0u64, |t, d| { *t += d; Some(*t) })
            .take_while(|t| *t <= 86_400).count() as u64;
        assert!(day < 300, "{day} dials in a day");
    }

    /// A host that stays down logs its error once an hour, not per retry; a
    /// different error is news and logs at once.
    #[test]
    fn a_repeated_dial_failure_is_logged_hourly() {
        let t0 = Instant::now();
        assert!(retry_log_due(None, "no route", t0));
        let logged = ("no route".to_string(), t0);
        assert!(!retry_log_due(Some(&logged), "no route", t0 + Duration::from_secs(300)));
        assert!(!retry_log_due(Some(&logged), "no route", t0 + Duration::from_secs(3599)));
        assert!(retry_log_due(Some(&logged), "no route", t0 + Duration::from_secs(3600)));
        assert!(retry_log_due(Some(&logged), "auth failed", t0 + Duration::from_secs(1)));
    }

    /// Pacing follows how far the dial got, not the wording of the error.
    #[test]
    fn failures_are_classified_by_how_far_the_dial_got() {
        // the machine never answered `herdr status`
        for e in [
            "ssh master to office failed: ssh: connect to host office port 22: Connection timed out",
            "ssh exec failed (exec herdr status --json): ssh timeout",
            "ssh master to office did not create a usable control socket: exit 255",
        ] {
            assert_eq!(classify(e, false), Failure::Unreachable, "{e}");
        }
        // it did, so what failed after is herdr: the 0.9.5 shutdown answer,
        // a socket that is gone, the server reported down, the stream cut
        for e in [
            "remote herdr server is not running",
            "remote herdr session \"work\" is not running",
            "ping: server is shutting down",
            "api closed before response: ping",
            "No such file or directory (os error 2)",
            "event stream closed",
            "subscribe: stream closed before ack",
        ] {
            assert_eq!(classify(e, true), Failure::HerdrDown, "{e}");
        }
        let dormant = format!("{}: no running container for app", crate::remote::DORMANT);
        assert_eq!(classify(&dormant, false), Failure::Dormant);
        assert_eq!(classify(&dormant, true), Failure::Dormant);
        // a user string that merely contains the marker is not dormant
        assert_eq!(classify("ssh master to dormant-box failed: exit 255", false), Failure::Unreachable);
    }

    /// A herdr restart on a reachable host is redialled every 2 s for 3 min,
    /// with one log line at the start; past that it joins the ladder.
    #[test]
    fn a_down_herdr_is_redialled_fast_for_a_bounded_window() {
        let t0 = Instant::now();
        let mut r = Retry::new();
        assert_eq!(r.next(Failure::HerdrDown, t0), (2, Pace::FastStart));
        let mut t = t0;
        let mut fast = 0;
        loop {
            t += Duration::from_secs(2);
            match r.next(Failure::HerdrDown, t) {
                (2, Pace::Fast) => fast += 1,
                (5, Pace::FastSpent) => break,
                other => panic!("unexpected {other:?} at {:?}", t - t0),
            }
        }
        assert_eq!(t - t0, HERDR_DOWN_WINDOW);
        assert_eq!(fast, 89, "one dial every 2 s for 3 min");
        assert_eq!(r.next(Failure::HerdrDown, t + Duration::from_secs(5)), (10, Pace::Slow));
        assert_eq!(r.next(Failure::HerdrDown, t + Duration::from_secs(15)), (20, Pace::Slow));

        // a sync opens a fresh window for the next restart
        r.connected();
        r.synced();
        assert_eq!(r.next(Failure::HerdrDown, t + Duration::from_secs(600)), (2, Pace::FastStart));
    }

    /// The outage that motivated the window: the stream closes, herdr is back
    /// 60 s later. The ladder answered at 75 s (5+10+20+40); the fast retry
    /// within one 2 s step of the server's return.
    #[test]
    fn a_restart_reconnects_within_one_fast_step() {
        let t0 = Instant::now();
        let back = Duration::from_secs(60);
        let mut r = Retry::new();
        let mut t = t0;
        loop {
            let (delay, _) = r.next(Failure::HerdrDown, t);
            t += Duration::from_secs(delay);
            if t - t0 >= back {
                break;
            }
        }
        assert!(t - t0 - back <= Duration::from_secs(HERDR_DOWN_RETRY), "{:?}", t - t0);
    }

    /// Unreachable keeps the ladder; it closes a fast window, so a host that
    /// comes back from a reboot with herdr still starting gets a fresh one;
    /// dormant waits do not climb.
    #[test]
    fn unreachable_and_dormant_keep_their_pacing() {
        let t0 = Instant::now();
        let mut r = Retry::new();
        assert_eq!(r.next(Failure::HerdrDown, t0), (2, Pace::FastStart));
        let ladder: Vec<u64> = (0..4).map(|_| r.next(Failure::Unreachable, t0).0).collect();
        assert_eq!(ladder, [5, 10, 20, 40]);
        assert_eq!(r.next(Failure::Dormant, t0), (DORMANT_DELAY, Pace::Slow));
        assert_eq!(r.next(Failure::Unreachable, t0), (80, Pace::Slow));
        let later = t0 + Duration::from_secs(3600);
        assert_eq!(r.next(Failure::HerdrDown, later), (2, Pace::FastStart));
        assert_eq!(r.next(Failure::HerdrDown, later + Duration::from_secs(2)), (2, Pace::Fast));
        r.connected();
        assert_eq!(r.next(Failure::Unreachable, later), (5, Pace::Slow));
    }

    /// A connection that answers but never syncs must not keep the 2 s
    /// cadence forever: only a sync reopens the window.
    #[test]
    fn a_connection_that_never_syncs_does_not_reopen_the_window() {
        let t0 = Instant::now();
        let mut r = Retry::new();
        assert_eq!(r.next(Failure::HerdrDown, t0), (2, Pace::FastStart));
        let mut t = t0;
        while t - t0 < HERDR_DOWN_WINDOW {
            t += Duration::from_secs(2);
            r.connected();
            r.next(Failure::HerdrDown, t);
        }
        r.connected();
        assert_eq!(r.next(Failure::HerdrDown, t + Duration::from_secs(2)), (5, Pace::Slow));
    }

    /// A sooner request wins; a later one never postpones a scheduled one.
    #[test]
    fn scheduling_keeps_the_earliest_deadline() {
        let t0 = Instant::now();
        let mut at = None;
        schedule(&mut at, t0 + Duration::from_secs(2));
        schedule(&mut at, t0 + Duration::from_secs(5));
        assert_eq!(at, Some(t0 + Duration::from_secs(2)));
        schedule(&mut at, t0);
        assert_eq!(at, Some(t0));
    }

    /// Hub events name the object a host would reconcile; everything else is
    /// none of any host's business.
    #[test]
    fn local_events_name_the_object_to_route_by() {
        let layout = json!({ "layout": { "tab_id": "w3:t2", "workspace_id": "w3", "panes": [] } });
        assert_eq!(local_event_target("layout_updated", &layout).as_deref(), Some("w3:t2"));
        assert_eq!(local_event_target("tab_renamed", &json!({ "tab_id": "w3:t1" })).as_deref(), Some("w3:t1"));
        assert_eq!(local_event_target("pane_closed", &json!({ "pane_id": "w3:p1" })).as_deref(), Some("w3:p1"));
        assert_eq!(local_event_target("workspace_closed", &json!({ "workspace_id": "w3" })).as_deref(), Some("w3"));
        assert_eq!(local_event_target("workspace_created", &json!({ "workspace_id": "w4" })), None);
    }

    /// Only the host whose map holds the id is poked; a hub-only object pokes
    /// nobody.
    #[test]
    fn local_events_poke_only_the_owning_host() {
        let env = lock_test_env("owners");
        let hosts: Vec<HostConfig> = crate::config::parse_config(
            "[hosts.a]\ntarget = \"a\"\n[hosts.b]\ntarget = \"b\"\n",
        )
        .unwrap()
        .hosts;
        let mut b = crate::state::HostState::default();
        b.tabs.insert(
            "w1:t1".into(),
            crate::state::TabEntry { local_id: "w9:t3".into(), last_remote_label: None },
        );
        save_state(&env.state_dir, "b", &b).unwrap();
        assert_eq!(owning_hosts(&env.state_dir, &hosts, "w9:t3"), vec![1]);
        assert!(owning_hosts(&env.state_dir, &hosts, "w2:t1").is_empty());
        let _ = fs::remove_dir_all(&env.state_dir);
    }

    /// A remote that doesn't know `pane.updated` rejects the whole subscribe,
    /// so the fallback list must really leave it out, and only that rejection
    /// may turn it off: a current server rejecting something else lists
    /// `pane.updated` among the variants it expected, and misreading that would
    /// silently stop live titles on a remote that supports them.
    #[test]
    fn title_events_are_dropped_only_for_a_remote_that_lacks_them() {
        let title_sub = json!({ "type": "pane.updated" });
        assert!(sub_list(true).contains(&title_sub));
        assert!(!sub_list(false).contains(&title_sub));

        assert!(rejects_title_events(
            "events.subscribe: invalid request: unknown variant `pane.updated`, expected one of \
             `workspace.created`, `pane.created`, `pane.agent_status_changed` at line 1 column 91"
        ));
        // captured from herdr 0.9.0, rejecting a different variant
        assert!(!rejects_title_events(
            "events.subscribe: invalid request: unknown variant `pane.title_changed`, expected one of \
             `pane.created`, `pane.closed`, `pane.updated`, `pane.focused`, `layout.updated` at line 1 column 91"
        ));
        assert!(!rejects_title_events("events.subscribe: pane w1:p9 not found"));
    }

    fn lock_test_env(tag: &str) -> Env {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_dir = std::env::temp_dir().join(format!(
            "herdr-mirror-daemon-lock-{tag}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&state_dir).unwrap();
        Env { config_search: Vec::new(), state_dir, local_socket: PathBuf::new() }
    }

    #[test]
    fn daemon_lifetime_lock_excludes_a_second_owner() {
        let env = lock_test_env("exclusive");
        let owner = acquire_daemon_lifetime(&env).unwrap();
        assert_eq!(running_pid(&env), Some(std::process::id() as i32));
        assert!(acquire_daemon_lifetime(&env).is_err());
        drop(owner);
        assert_eq!(running_pid(&env), None);
        let _ = fs::remove_dir_all(&env.state_dir);
    }

    #[test]
    fn locked_owner_is_recovered_when_pidfile_disappears() {
        let env = lock_test_env("missing-pidfile");
        let owner = acquire_daemon_lifetime(&env).unwrap();
        fs::remove_file(pid_path(&env)).unwrap();
        assert_eq!(running_pid(&env), Some(std::process::id() as i32));
        drop(owner);
        let _ = fs::remove_dir_all(&env.state_dir);
    }

    #[test]
    fn lifetime_guard_does_not_remove_a_newer_pidfile() {
        let env = lock_test_env("new-owner");
        let owner = acquire_daemon_lifetime(&env).unwrap();
        fs::write(pid_path(&env), "123456").unwrap();
        drop(owner);
        assert_eq!(fs::read_to_string(pid_path(&env)).unwrap(), "123456");
        let _ = fs::remove_dir_all(&env.state_dir);
    }

    /// One fallback is not evidence: the probe also fails when the remote herdr
    /// is restarting or the mux hiccups, and pinning a healthy host to the
    /// slower transport for the daemon's life is worse than re-probing once.
    #[test]
    fn a_single_fallback_is_not_remembered() {
        use crate::config::ApiTransport;
        let mut streak = 0u32;

        // transient: fell back once, then the forward worked again
        assert_eq!(remember_transport(Some(ApiTransport::Exec), &mut streak), None);
        assert_eq!(remember_transport(Some(ApiTransport::Socket), &mut streak), Some(ApiTransport::Socket));
        assert_eq!(streak, 0);

        // genuinely broken: two in a row sticks, and stays stuck
        assert_eq!(remember_transport(Some(ApiTransport::Exec), &mut streak), None);
        assert_eq!(
            remember_transport(Some(ApiTransport::Exec), &mut streak),
            Some(ApiTransport::Exec)
        );
        assert_eq!(
            remember_transport(Some(ApiTransport::Exec), &mut streak),
            Some(ApiTransport::Exec)
        );

        // a later success clears it, so a fixed host returns to the forward
        assert_eq!(remember_transport(Some(ApiTransport::Socket), &mut streak), Some(ApiTransport::Socket));
        assert_eq!(remember_transport(Some(ApiTransport::Exec), &mut streak), None);
    }

    fn argv(parts: &[&str]) -> Vec<Value> {
        parts.iter().map(|s| json!(s)).collect()
    }

    /// Real argv, captured from `pane.process_info` on a live ssh mirror pane.
    #[test]
    fn recognises_a_live_streamer() {
        let streamer = argv(&[
            "/Users/niko/Documents/coding/herdr-mirror/target/release/herdr-mirror",
            "pane",
            "vps",
            "wC:p1",
            "--remote-bin",
            "~/.local/bin/herdr",
        ]);
        assert!(is_streamer_argv(&streamer));

        // the ssh child sharing the same pane is not itself a streamer
        let ssh_child = argv(&["ssh", "-o", "BatchMode=yes", "vps", "exec ~/.local/bin/herdr ..."]);
        assert!(!is_streamer_argv(&ssh_child));
    }

    /// A docker pane's wrapper looks the same to this check — the whole point
    /// of asking herdr per pane instead of matching transport-specific flags.
    #[test]
    fn transport_and_flags_are_irrelevant() {
        assert!(is_streamer_argv(&argv(&[
            "/plugins/github/mirror-0015/target/release/herdr-mirror",
            "pane",
            "/Users/n/proj",
            "w1:p1",
            "--container-folder",
            "/Users/n/proj",
        ])));
        // and a pre-v0.1.7 streamer, which carried no identity flag at all
        assert!(is_streamer_argv(&argv(&["/usr/local/bin/herdr-mirror", "pane", "vps", "w1:p1"])));
    }

    /// A shell left behind by session-restore is what healing must act on.
    #[test]
    fn plain_shell_is_not_a_streamer() {
        assert!(!is_streamer_argv(&argv(&["-zsh"])));
        assert!(!is_streamer_argv(&argv(&["/bin/bash"])));
        assert!(!is_streamer_argv(&argv(&[])));
    }

    /// Another subcommand in the pane must not read as a live stream.
    #[test]
    fn other_subcommands_are_not_streamers() {
        assert!(!is_streamer_argv(&argv(&["/usr/local/bin/herdr-mirror", "status"])));
        assert!(!is_streamer_argv(&argv(&["/usr/local/bin/herdr-mirror"])));
    }

    /// The live failure reported no foreground streamer while its pidfile
    /// already named the active wrapper. Recovery must trust either signal.
    #[test]
    fn a_live_pidfile_blocks_false_recovery() {
        assert!(!streamer_recovery_needed(Some(false), true));
        assert!(!streamer_recovery_needed(Some(true), false));
        assert!(!streamer_recovery_needed(None, false));
        assert!(streamer_recovery_needed(Some(false), false));
    }
}
