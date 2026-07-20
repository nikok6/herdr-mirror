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
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::api::{ApiClient, EventStream};
use crate::config::{load_config, HostConfig};
use crate::mirror::{
    apply_remote_closes, converge, mark_unknown, mirror_source, push_pane_status, regroup_sidebar,
    teardown, AgentInfo, ConvergeDeps,
};
use crate::state::{load_state, save_state, HostState};
use crate::util::{err, now_iso, pid_alive, sleep_until_earliest, Env, Logger, Result};

// --- pidfile / pause marker ---

fn pid_path(env: &Env) -> PathBuf {
    env.state_dir.join("daemon.pid")
}

pub fn running_pid(env: &Env) -> Option<i32> {
    let pid: i32 = fs::read_to_string(pid_path(env)).ok()?.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
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
}

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
];

fn sub_list(pane_ids: &[String]) -> Vec<Value> {
    let mut subs: Vec<Value> = BROADCAST_SUBS.iter().map(|t| json!({ "type": t })).collect();
    subs.extend(pane_ids.iter().map(|p| json!({ "type": "pane.agent_status_changed", "pane_id": p })));
    subs
}

/// Broadcast structure events + per-pane agent-status subscriptions
/// (pane.agent_status_changed requires a pane_id). A rejected pane
/// subscription degrades to broadcast-only instead of killing the connection.
async fn resubscribe(
    ctx: &HostCtx,
    remote: &ApiClient,
    stream: &mut EventStream,
    subscribed_key: &mut String,
    state: &HostState,
) -> Result<()> {
    // live panes only: tombstoned mirrors' statuses are moot
    let mut pane_ids: Vec<String> = state
        .panes
        .iter()
        .filter(|(_, e)| !e.is_tombstoned())
        .map(|(rid, _)| rid.clone())
        .collect();
    pane_ids.sort();
    let key = pane_ids.join(",");
    if key == *subscribed_key {
        return Ok(());
    }
    match remote.subscribe(sub_list(&pane_ids)).await {
        Ok(s) => {
            *stream = s;
            *subscribed_key = key;
            Ok(())
        }
        Err(e) => {
            ctx.log.log(&format!(
                "[{}] pane subscriptions rejected ({e}) — broadcast only",
                ctx.host.name
            ));
            *stream = remote.subscribe(sub_list(&[])).await?;
            *subscribed_key = "<broadcast>".into();
            Ok(())
        }
    }
}

/// Fast-path: apply coalesced status updates without a remote snapshot.
/// Returns true if an event referenced a pane we don't mirror yet.
async fn flush_status(ctx: &HostCtx, pending: HashMap<String, Value>) -> bool {
    let mut state = load_state(&ctx.env_state_dir, &ctx.host.name);
    let mut need_converge = false;
    for (remote_id, data) in pending {
        let Some(entry) = state.panes.get_mut(&remote_id) else {
            need_converge = true; // unknown pane → let a full pass create it
            continue;
        };
        if entry.is_tombstoned() {
            continue; // user closed this mirror — its statuses are moot
        }
        let info: AgentInfo = serde_json::from_value(data).unwrap_or_default();
        let agent = info.has_agent().then_some(&info);
        push_pane_status(&ctx.local, &ctx.host.name, &remote_id, entry, agent, &ctx.log).await;
    }
    if let Err(e) = save_state(&ctx.env_state_dir, &ctx.host.name, &state) {
        ctx.log.log(&format!("[{}] state save failed: {e}", ctx.host.name));
    }
    need_converge
}

/// Connected phase: subscribe, converge, then react to events/pokes/timers
/// until the connection drops (returns Err).
async fn run_connected(
    ctx: &HostCtx,
    poke: &mut mpsc::Receiver<()>,
    backoff_idx: &mut usize,
) -> Result<()> {
    let mut remote_host = crate::remote::RemoteHost::new(&ctx.host, &ctx.env_state_dir);
    let (remote, _status) = remote_host.connect_api().await?;
    *backoff_idx = 0;
    let deps = ConvergeDeps {
        local: ctx.local.clone(),
        remote: remote.clone(),
        host: ctx.host.clone(),
        state_dir: ctx.env_state_dir.clone(),
        log: ctx.log.clone(),
        close_remote_on_local_close: ctx.close_remote_on_local_close,
        closes: ctx.closes.clone(),
    };
    // broadcast-only first: subscribing a since-dead pane id is rejected, so
    // converge must prune the map before the per-pane upgrade
    let mut stream = remote.subscribe(sub_list(&[])).await?;
    let mut subscribed_key = String::from("<broadcast>");
    let state = converge(&deps).await?;
    resubscribe(ctx, &remote, &mut stream, &mut subscribed_key, &state).await?;
    ctx.log.log(&format!("[{}] connected and synced", ctx.host.name));

    let mut converge_at: Option<Instant> = None;
    let mut status_at: Option<Instant> = None;
    let mut closes_at: Option<Instant> = None;
    let mut pending_status: HashMap<String, Value> = HashMap::new();
    let mut pending_closes: Vec<String> = Vec::new();

    loop {
        let sleep = sleep_until_earliest([converge_at, status_at, closes_at]);
        tokio::select! {
            ev = stream.next() => {
                match ev {
                    None => return Err(err("event stream closed")),
                    // status changes take the fast-path; structure changes
                    // need a full reconcile (debounced 500ms)
                    Some(e) if e.event == "pane_agent_status_changed" => {
                        if let Some(pid) = e.data.get("pane_id").and_then(|v| v.as_str()) {
                            // coalesce: keep only the latest per pane
                            pending_status.insert(pid.to_string(), e.data.clone());
                            status_at.get_or_insert(Instant::now() + Duration::from_millis(150));
                        }
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
                            closes_at.get_or_insert(Instant::now() + Duration::from_millis(150));
                        }
                    }
                    Some(_) => {
                        converge_at.get_or_insert(Instant::now() + Duration::from_millis(500));
                    }
                }
            }
            Some(()) = poke.recv() => {
                converge_at.get_or_insert(Instant::now());
            }
            _ = sleep => {
                let now = Instant::now();
                if status_at.is_some_and(|t| t <= now) {
                    status_at = None;
                    let pending = std::mem::take(&mut pending_status);
                    if flush_status(ctx, pending).await {
                        // unknown pane → let a full pass create it
                        converge_at.get_or_insert(now);
                    }
                }
                if closes_at.is_some_and(|t| t <= now) {
                    closes_at = None;
                    let closed = std::mem::take(&mut pending_closes);
                    apply_remote_closes(&ctx.local, &ctx.env_state_dir, &ctx.host.name, &closed, &ctx.log).await;
                    // reconcile + refresh subscriptions after the removals
                    converge_at.get_or_insert(now);
                }
                if converge_at.is_some_and(|t| t <= now) {
                    converge_at = None;
                    let state = converge(&deps).await?;
                    // pane set may have changed
                    resubscribe(ctx, &remote, &mut stream, &mut subscribed_key, &state).await?;
                }
            }
        }
    }
}

async fn host_task(ctx: HostCtx, mut poke: mpsc::Receiver<()>) {
    let mut backoff_idx = 0usize;
    loop {
        let e = match run_connected(&ctx, &mut poke, &mut backoff_idx).await {
            Ok(()) => unreachable!("run_connected only returns on error"),
            Err(e) => e,
        };
        mark_unknown(&ctx.local, &ctx.env_state_dir, &ctx.host.name, "mirror: ssh lost").await;
        let delays = [5u64, 10, 30];
        let delay = delays[backoff_idx.min(delays.len() - 1)];
        backoff_idx += 1;
        ctx.log.log(&format!("[{}] disconnected ({e}) — retrying in {delay}s", ctx.host.name));
        tokio::time::sleep(Duration::from_secs(delay)).await;
        // drain stale pokes accumulated while down (reconnect converges anyway)
        while poke.try_recv().is_ok() {}
    }
}

/// After a local herdr server restart, session-restore resurrects mirror panes
/// as plain shells: their ids match the map, but no streamer processes exist —
/// and converge can't tell (the snapshot has no process info), so the mirrors
/// sit frozen forever. The tell is the conjunction: mapped panes present, zero
/// wrapper processes for the host. Heal = teardown + poke; the next converge
/// rebuilds every mirror with a live streamer. A transient socket blip leaves
/// wrappers running, so the check stays quiet then.
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
        let streamers = std::process::Command::new("pgrep")
            .args(["-f", &format!("herdr-mirror pane {}", h.target)])
            .output()
            .map(|o| o.stdout.iter().filter(|&&b| b == b'\n').count())
            .unwrap_or(0);
        if streamers > 0 {
            continue;
        }
        log.log(&format!(
            "[{}] {} mirror pane(s) mapped but no streamers running (server restart?) — re-exec'ing streamers",
            h.name,
            panes.len()
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
        for (remote_pane_id, local_pane_id) in panes {
            let argv = cmd_for(&remote_pane_id);
            crate::mirror::spawn_streamer_pane(local, &local_pane_id, &argv, log).await;
        }
        let _ = pokers[i].try_send(());
    }
}

// Local events: mirror closes drive tombstoning — poke every host so the
/// next converge records the user's intent promptly.
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
        ];
        match local.subscribe(subs).await {
            Ok(mut stream) => {
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
                        _ => None,
                    };
                    if let Some(k) = key {
                        if let Some(lid) = e.data.get(k).and_then(|v| v.as_str()) {
                            if let Ok(mut t) = closes.lock() {
                                t.note_close_event(lid);
                            }
                        }
                    }
                    for p in &pokers {
                        let _ = p.try_send(());
                    }
                    // a workspace appeared/left — keep hosts grouped (no-op if already)
                    regroup_sidebar(&local, &prefixes, &log).await;
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
    let detached = std::env::var("HERDR_MIRROR_DETACHED").is_ok();
    let log = Logger::new(&env.state_dir, !detached);
    let config = load_config(&env.config_search)?;
    fs::write(pid_path(&env), std::process::id().to_string())?;
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

    let local = ApiClient::connect(&env.local_socket).await?;
    let closes = crate::closes::new_closes();
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
    let _ = fs::remove_file(pid_path(&env));
    Ok(())
}

pub fn cmd_start(env: &Env) -> Result<()> {
    // flock + parent-written pidfile: two racing starts (focus hook) must not
    // both see "not running" and spawn duplicate daemons
    use std::os::fd::AsRawFd;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(env.state_dir.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(err("cannot lock daemon.lock"));
    }
    if running_pid(env).is_some() {
        println!("mirror daemon already running");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env.state_dir.join("daemon.log"))?;
    let log2 = log.try_clone()?;
    use std::os::unix::process::CommandExt;
    let child = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log2)
        .env("HERDR_MIRROR_DETACHED", "1")
        .process_group(0)
        .spawn()?;
    fs::write(pid_path(env), child.id().to_string())?;
    println!("mirror daemon started (pid {})", child.id());
    Ok(())
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
    let config = load_config(&env.config_search)?;
    if let Some(src) = &config.source {
        println!("config: {}", src.display());
    }
    for ignored in &config.shadowed {
        println!("warning: ignoring shadowed config at {}", ignored.display());
    }
    for h in &config.hosts {
        let state = load_state(&env.state_dir, &h.name);
        let ws = state.workspaces.values().filter(|w| !w.is_tombstoned()).count();
        let panes = state.panes.values().filter(|p| !p.is_tombstoned()).count();
        println!("host {} ({}): {ws} mirror workspaces, {panes} mirror panes", h.name, h.target);
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
