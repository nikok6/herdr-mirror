// Reconciliation: project a remote herdr server's workspaces/tabs/panes into
// the local server as `prefix:*` mirror objects, and push the remote's
// authoritative agent statuses onto the mirror panes.
//
// The id map (src/state.rs, persisted per host) distinguishes "user closed the
// mirror locally" (tombstone — don't recreate) from "remote object went away"
// (close the mirror).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::ApiClient;
use crate::config::HostConfig;
use crate::state::{load_state, save_state, HostState, PaneEntry, WsEntry};
use crate::util::{Logger, Result};

// --- snapshot shapes (subset of the API's SessionSnapshot) ---

#[derive(Debug, Clone, Deserialize)]
pub struct WsInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    pub tab_count: Option<u64>,
    pub pane_count: Option<u64>,
    pub active_tab_id: Option<String>,
    /// custom metadata tokens the remote publishes. `default` on purpose: a
    /// pre-0.7.4 remote never sends this.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    /// unread today, but part of the pane wire shape — kept so the struct
    /// documents what the API actually returns
    #[allow(dead_code)]
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
}

/// Agent fields as they appear both in snapshot `agents[]` and in
/// `pane.agent_status_changed` event data (null fields omitted there).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub pane_id: String,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    // snapshot agents carry this as `name`; the pane_agent_status_changed event
    // carries the same title as `title`, so accept either
    #[serde(alias = "title")]
    pub name: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    pub custom_status: Option<String>,
    pub state_labels: Option<BTreeMap<String, String>>,
    /// custom metadata tokens the remote publishes ($model, $summary, …).
    /// `default` on purpose: a pre-0.7.4 remote never sends this.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
}

impl AgentInfo {
    /// Does this describe a live agent (vs. a sparse release event)?
    pub fn has_agent(&self) -> bool {
        self.agent.as_deref().is_some_and(|a| !a.is_empty())
            || self.agent_status.as_deref().is_some_and(|s| s != "unknown")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutRect {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutPaneSnapshot {
    pub pane_id: String,
    pub rect: LayoutRect,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutSnapshot {
    #[allow(dead_code)]
    pub tab_id: String,
    #[serde(default)]
    pub panes: Vec<LayoutPaneSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub workspaces: Vec<WsInfo>,
    #[serde(default)]
    pub tabs: Vec<TabInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub layouts: Vec<LayoutSnapshot>,
}

pub async fn fetch_snapshot(api: &ApiClient) -> Result<Snapshot> {
    #[derive(Deserialize)]
    struct Res {
        snapshot: Snapshot,
    }
    let res: Res = api.request_t("session.snapshot", json!({})).await?;
    Ok(res.snapshot)
}

// --- layout tree ---

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutNode {
    Pane {
        pane_id: Option<String>,
        label: Option<String>,
    },
    Split {
        direction: String,
        ratio: f64,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// Locate `pane_id` in a split tree: the parent split's direction (already
/// "right"/"down", pane.split's vocabulary) plus the sibling subtree's pane
/// ids ordered nearest-to-the-split-point first. This is exact — the tree
/// records how the panes were actually split, so no geometry heuristics.
pub fn locate_in_layout(node: &LayoutNode, pane_id: &str) -> Option<(String, Vec<String>)> {
    let LayoutNode::Split { direction, first, second, .. } = node else { return None };
    let is_the_pane =
        |n: &LayoutNode| matches!(n, LayoutNode::Pane { pane_id: Some(p), .. } if p == pane_id);
    if is_the_pane(first) {
        let mut sibs = Vec::new();
        walk_pane_ids(second, &mut sibs);
        return Some((direction.clone(), sibs));
    }
    if is_the_pane(second) {
        let mut sibs = Vec::new();
        walk_pane_ids(first, &mut sibs);
        sibs.reverse();
        return Some((direction.clone(), sibs));
    }
    locate_in_layout(first, pane_id).or_else(|| locate_in_layout(second, pane_id))
}

fn walk_pane_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Pane { pane_id, .. } => out.push(pane_id.clone().unwrap_or_default()),
        LayoutNode::Split { first, second, .. } => {
            walk_pane_ids(first, out);
            walk_pane_ids(second, out);
        }
    }
}

/// Layout tree as plain shell panes (no `command`, so herdr won't set
/// `launch_argv` and treat them as agents); the streamer is exec'd in afterward.
fn map_node(node: &LayoutNode, cwd: &str) -> Value {
    match node {
        LayoutNode::Pane { pane_id: _, label } => json!({
            "type": "pane",
            "label": label,
            "cwd": cwd,
        }),
        LayoutNode::Split { direction, ratio, first, second } => json!({
            "type": "split",
            "direction": direction,
            "ratio": ratio,
            "first": map_node(first, cwd),
            "second": map_node(second, cwd),
        }),
    }
}

/// Mark a local id the plugin itself is about to close, so the close event it
/// raises isn't read back as the user closing the mirror (see closes.rs).
fn mark_self_close(deps: &ConvergeDeps, local_id: &str) {
    if let Ok(mut t) = deps.closes.lock() {
        t.mark_self_close(local_id);
    }
}

fn map_status(remote: &str) -> &'static str {
    match remote {
        "working" => "working",
        "blocked" => "blocked",
        "idle" => "idle",
        // local herdr derives "done" from working→idle while unseen
        "done" => "idle",
        _ => "unknown",
    }
}

pub fn mirror_source(host_name: &str) -> String {
    format!("plugin:mirror:{host_name}")
}

/// The server rejects custom_status longer than this.
const CUSTOM_STATUS_MAX: usize = 32;

fn clamp_status(s: &str) -> String {
    s.chars().take(CUSTOM_STATUS_MAX).collect()
}

// Observe requests = the remote pane's real size + a margin that absorbs
// modest remote resizes (a larger resize clips until the wrapper reconnects).
const OBSERVE_MARGIN_COLS: u32 = 16;
const OBSERVE_MARGIN_ROWS: u32 = 8;

/// How to resolve a mirror-workspace label state.
#[derive(Debug, PartialEq)]
enum LabelAction {
    /// labels agree — nothing to do
    InSync,
    /// user renamed the mirror locally → rename the REMOTE workspace to this
    PushRemote(String),
    /// remote is the authority (remote renamed, or unknown history) → restamp local
    RestampLocal,
}

/// Two-way rename resolution. `last_remote` is the remote label as of the
/// previous converge (None = pre-upgrade state file / first sight: remote wins).
fn resolve_ws_label(
    prefix: &str,
    remote_label: &str,
    local_label: &str,
    last_remote: Option<&str>,
) -> LabelAction {
    let expected = format!("{prefix}: {remote_label}");
    if local_label == expected {
        return LabelAction::InSync;
    }
    if last_remote != Some(remote_label) {
        // remote changed since we last stamped (or no history) — remote wins
        return LabelAction::RestampLocal;
    }
    // remote unchanged, local differs → this is a user rename. Accept it with
    // or without the "<prefix>: " convention; empty/degenerate names restamp.
    let stripped =
        local_label.strip_prefix(&format!("{prefix}: ")).unwrap_or(local_label).trim();
    if stripped.is_empty() || stripped == remote_label {
        LabelAction::RestampLocal
    } else {
        LabelAction::PushRemote(stripped.to_string())
    }
}

pub struct ConvergeDeps {
    pub local: ApiClient,
    pub remote: ApiClient,
    pub host: HostConfig,
    pub state_dir: PathBuf,
    pub log: Logger,
    /// mirror closing a workspace/pane locally onto the remote (see MirrorConfig)
    pub close_remote_on_local_close: bool,
    /// event-confirmed local closes. Absence from the local snapshot is
    /// ambiguous (rebuild in flight, failed converge, server restart), so only a
    /// close event that wasn't our own may close the remote.
    pub closes: crate::closes::Closes,
}

/// argv for one mirror pane: this same binary in `pane` mode. Panes without a
/// known size get no --cols/--rows (the wrapper falls back to a default).
pub(crate) fn cmd_for_pane(
    host: &HostConfig,
    state_dir: &std::path::Path,
    sizes: &HashMap<String, LayoutRect>,
) -> impl Fn(&str) -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "herdr-mirror".into());
    let target = host.target.clone();
    let remote_bin = host.remote_bin.clone();
    let always_control = host.always_control;
    let kind = host.kind.clone();
    let docker_bin = host.docker_bin.clone();
    // daemon's ControlMaster socket for this host (see remote.rs); the streamer
    // reuses it for cheap foreground polls
    let ctl_path = state_dir.join(format!("{}.ctl", host.name)).display().to_string();
    let sizes = sizes.clone();
    move |pane_id: &str| {
        let mut argv = vec![
            exe.clone(),
            "pane".into(),
            target.clone(),
            pane_id.to_string(),
            "--remote-bin".into(),
            remote_bin.clone(),
        ];
        if always_control {
            argv.push("--always-control".into());
        }
        // ssh only: the pane reuses the daemon's ControlMaster for cheap
        // foreground polls. Docker has no ControlMaster, and healing no longer
        // needs a host-identity token in the argv at all — it asks herdr what
        // is running in each pane instead (see daemon::has_live_streamer).
        match &kind {
            crate::config::HostKind::Ssh => {
                argv.extend(["--ctl-path".into(), ctl_path.clone()]);
            }
            crate::config::HostKind::DockerContainer(name) => {
                argv.extend(["--container".into(), name.clone()]);
                argv.extend(["--docker-bin".into(), docker_bin.clone()]);
            }
            crate::config::HostKind::DockerFolder(folder) => {
                argv.extend(["--container-folder".into(), folder.clone()]);
                argv.extend(["--docker-bin".into(), docker_bin.clone()]);
            }
        }
        if let Some(rect) = sizes.get(pane_id) {
            argv.extend([
                "--cols".into(),
                (rect.width + OBSERVE_MARGIN_COLS).to_string(),
                "--rows".into(),
                (rect.height + OBSERVE_MARGIN_ROWS).to_string(),
            ]);
        }
        argv
    }
}

/// single-quote for a POSIX shell command line
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Exec the streamer into an already-created plain pane. Not `agent.start` (or a
/// layout `command`), which set `launch_argv` and would surface every mirror pane
/// as an agent row; a shell `exec` keeps it non-agent until a real agent is
/// reported onto it.
pub(crate) async fn spawn_streamer_pane(local: &ApiClient, local_pane_id: &str, argv: &[String], log: &Logger) {
    let line = format!(
        "exec {}\n",
        argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")
    );
    if let Err(e) = local
        .request("pane.send_text", json!({ "pane_id": local_pane_id, "text": line }))
        .await
    {
        log.log(&format!("spawn streamer {local_pane_id}: {e}"));
    }
}

/// cwd every mirror pane runs in, doubling as the loop-guard marker: it's set at
/// pane creation so it's in the snapshot immediately (no exec race), and its name
/// can't collide with a real dir.
const MIRROR_CWD_MARKER: &str = ".mirror-pane";

fn mirror_pane_cwd(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join(MIRROR_CWD_MARKER)
}

/// Is this remote pane another herdr-mirror's streamer pane? Read from the
/// snapshot cwd marker — free, and race-free.
fn pane_is_mirror(p: &PaneInfo) -> bool {
    let is_marker = |c: &Option<String>| {
        c.as_deref()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|f| f.to_str())
            == Some(MIRROR_CWD_MARKER)
    };
    is_marker(&p.foreground_cwd) || is_marker(&p.cwd)
}

// --- the converge pass ---

/// Returns the post-converge state so callers don't re-read the state file.
pub async fn converge(deps: &ConvergeDeps) -> Result<HostState> {
    let mut state = load_state(&deps.state_dir, &deps.host.name);
    let result = converge_inner(deps, &mut state).await;
    // save even on error: a crash mid-pass must not orphan created mirrors
    save_state(&deps.state_dir, &deps.host.name, &state)?;
    result.map(|()| state)
}

async fn converge_inner(deps: &ConvergeDeps, state: &mut HostState) -> Result<()> {
    let host = &deps.host;
    let log = &deps.log;
    let (remote_snap, local_snap) =
        tokio::try_join!(fetch_snapshot(&deps.remote), fetch_snapshot(&deps.local))?;

    let mut local_ws_ids: HashSet<String> =
        local_snap.workspaces.iter().map(|w| w.workspace_id.clone()).collect();
    let local_tab_ids: HashSet<&str> = local_snap.tabs.iter().map(|t| t.tab_id.as_str()).collect();
    let local_pane_ids: HashSet<&str> = local_snap.panes.iter().map(|p| p.pane_id.as_str()).collect();
    let remote_ws_ids: HashSet<&str> = remote_snap.workspaces.iter().map(|w| w.workspace_id.as_str()).collect();
    let remote_tab_ids: HashSet<&str> = remote_snap.tabs.iter().map(|t| t.tab_id.as_str()).collect();
    let remote_pane_ids: HashSet<&str> = remote_snap.panes.iter().map(|p| p.pane_id.as_str()).collect();
    let mut sizes: HashMap<String, LayoutRect> = HashMap::new();
    for layout in &remote_snap.layouts {
        for p in &layout.panes {
            sizes.insert(p.pane_id.clone(), p.rect.clone());
        }
    }
    let cmd_for = cmd_for_pane(&deps.host, &deps.state_dir, &sizes);
    let _ = std::fs::create_dir_all(mirror_pane_cwd(&deps.state_dir));

    // 1. detect mirrors that are gone locally. Always tombstone (never remove)
    //    so this pass can't recreate them (the snapshot still lists the object);
    //    section 2 reaps the tombstoned entry once the remote is gone.
    //
    //    Closing the REMOTE is destructive, so it is driven only by an
    //    event-confirmed user close (see closes.rs) — never by absence alone,
    //    which also happens mid-rebuild, after a failed converge, or while the
    //    local server is restarting.
    let close_remote = deps.close_remote_on_local_close;
    let mine: HashSet<String> = state
        .workspaces
        .values()
        .map(|e| e.local_id.clone())
        .chain(state.panes.values().map(|e| e.local_id.clone()))
        .collect();
    let user_closed = match deps.closes.lock() {
        Ok(mut t) => t.take_user_closed(&mine),
        Err(_) => HashSet::new(),
    };
    let mut ws_close_remote: Vec<String> = Vec::new();
    for (rid, entry) in state.workspaces.iter_mut() {
        if !entry.is_tombstoned() && !local_ws_ids.contains(&entry.local_id) && remote_ws_ids.contains(rid.as_str()) {
            entry.tombstone = Some(true);
            if close_remote && user_closed.contains(&entry.local_id) {
                ws_close_remote.push(rid.clone());
            } else {
                log.log(&format!("workspace mirror for {rid} was closed locally — tombstoning"));
            }
        }
    }
    for rid in &ws_close_remote {
        log.log(&format!("workspace mirror for {rid} closed locally — closing remote workspace"));
        if let Err(e) = deps.remote.request("workspace.close", json!({ "workspace_id": rid })).await {
            log.log(&format!("remote workspace close failed for {rid}: {e}"));
        }
    }
    let pane_ws: HashMap<&str, &str> =
        remote_snap.panes.iter().map(|p| (p.pane_id.as_str(), p.workspace_id.as_str())).collect();
    let mut drop_panes: Vec<String> = Vec::new();
    let mut pane_close_remote: Vec<String> = Vec::new();
    for (rid, entry) in state.panes.iter_mut() {
        if !entry.is_tombstoned() && !local_pane_ids.contains(entry.local_id.as_str()) && remote_pane_ids.contains(rid.as_str()) {
            let ws_entry = pane_ws.get(rid.as_str()).and_then(|ws| state.workspaces.get(*ws));
            // if the pane's whole mirror workspace is gone, the stale pane
            // entry is collateral — drop it (its tombstoned workspace already
            // blocks recreation)
            match ws_entry {
                Some(w) if !w.is_tombstoned() && local_ws_ids.contains(&w.local_id) => {
                    entry.tombstone = Some(true);
                    if close_remote && user_closed.contains(&entry.local_id) {
                        pane_close_remote.push(rid.clone());
                    } else {
                        log.log(&format!("pane mirror for {rid} was closed locally — tombstoning"));
                    }
                }
                _ => drop_panes.push(rid.clone()),
            }
        }
    }
    for rid in &pane_close_remote {
        log.log(&format!("pane mirror for {rid} closed locally — closing remote pane"));
        if let Err(e) = deps.remote.request("pane.close", json!({ "pane_id": rid })).await {
            log.log(&format!("remote pane close failed for {rid}: {e}"));
        }
    }
    for rid in drop_panes {
        state.panes.remove(&rid);
    }

    // 2. remote objects that disappeared → close their mirrors. Explicit
    //    `*.closed` events are the authoritative close path (see apply_remote_closes);
    //    this snapshot-absence sweep is only a backstop for missed events, and it
    //    acts only when the object was ALSO absent last pass — so a remote that
    //    reconnected mid-restore (transiently empty/partial snapshot) can't
    //    mass-close mirrors.
    let prev_ids = std::mem::take(&mut state.prev_remote_ids);
    let absent_twice = |rid: &str, present: &HashSet<&str>| {
        !present.contains(rid) && !prev_ids.contains(rid)
    };
    let gone_ws: Vec<String> =
        state.workspaces.keys().filter(|rid| absent_twice(rid, &remote_ws_ids)).cloned().collect();
    for rid in gone_ws {
        let entry = state.workspaces.remove(&rid).unwrap();
        if !entry.is_tombstoned() && local_ws_ids.contains(&entry.local_id) {
            log.log(&format!("remote workspace {rid} gone — closing mirror {}", entry.local_id));
            mark_self_close(deps, &entry.local_id);
            if let Err(e) = deps.local.request("workspace.close", json!({ "workspace_id": entry.local_id })).await {
                log.log(&format!("close failed: {e}"));
            }
        }
    }
    let gone_tabs: Vec<String> =
        state.tabs.keys().filter(|rid| absent_twice(rid, &remote_tab_ids)).cloned().collect();
    for rid in gone_tabs {
        let entry = state.tabs.remove(&rid).unwrap();
        if local_tab_ids.contains(entry.local_id.as_str()) {
            let _ = deps.local.request("tab.close", json!({ "tab_id": entry.local_id })).await;
        }
    }
    let gone_panes: Vec<String> =
        state.panes.keys().filter(|rid| absent_twice(rid, &remote_pane_ids)).cloned().collect();
    for rid in gone_panes {
        let entry = state.panes.remove(&rid).unwrap();
        if !entry.is_tombstoned() && local_pane_ids.contains(entry.local_id.as_str()) {
            mark_self_close(deps, &entry.local_id);
            let _ = deps.local.request("pane.close", json!({ "pane_id": entry.local_id })).await;
        }
    }
    // record this pass's remote ids for the next comparison
    state.prev_remote_ids = remote_ws_ids
        .iter()
        .chain(remote_tab_ids.iter())
        .chain(remote_pane_ids.iter())
        .map(|s| s.to_string())
        .collect();

    // skip remote workspaces that are entirely another herdr-mirror's streamer
    // panes (a machine mirroring us back), so mutual mirroring can't nest.
    let mut panes_by_ws: HashMap<&str, Vec<&PaneInfo>> = HashMap::new();
    for p in &remote_snap.panes {
        panes_by_ws.entry(p.workspace_id.as_str()).or_default().push(p);
    }
    let mut mirror_ws_ids: HashSet<String> = HashSet::new();
    for rws in &remote_snap.workspaces {
        let Some(panes) = panes_by_ws.get(rws.workspace_id.as_str()).filter(|p| !p.is_empty()) else {
            continue;
        };
        if panes.iter().all(|p| pane_is_mirror(p)) {
            mirror_ws_ids.insert(rws.workspace_id.clone());
        }
    }

    // 3. remote workspaces → ensure mirrors exist with the right label
    for rws in &remote_snap.workspaces {
        if mirror_ws_ids.contains(&rws.workspace_id) {
            continue;
        }
        let label = format!("{}: {}", host.prefix, rws.label);
        if state.workspaces.get(&rws.workspace_id).is_some_and(|e| e.is_tombstoned()) {
            continue;
        }
        let existing = state
            .workspaces
            .get(&rws.workspace_id)
            .filter(|e| local_ws_ids.contains(&e.local_id))
            .cloned();
        if let Some(entry) = existing {
            let local_ws = local_snap.workspaces.iter().find(|w| w.workspace_id == entry.local_id);
            if let Some(lws) = local_ws {
                match resolve_ws_label(&host.prefix, &rws.label, &lws.label, entry.last_remote_label.as_deref()) {
                    LabelAction::PushRemote(new_remote) => {
                        // the user renamed the mirror → the rename is intent for
                        // the REMOTE workspace; push it there and restamp local
                        // with the canonical "<prefix>: <name>" form
                        log.log(&format!(
                            "local rename of {} → pushing \"{new_remote}\" to remote {}",
                            lws.label, rws.workspace_id
                        ));
                        deps.remote
                            .request(
                                "workspace.rename",
                                json!({ "workspace_id": rws.workspace_id, "label": new_remote }),
                            )
                            .await?;
                        let stamped = format!("{}: {}", host.prefix, new_remote);
                        if lws.label != stamped {
                            deps.local
                                .request("workspace.rename", json!({ "workspace_id": entry.local_id, "label": stamped }))
                                .await?;
                        }
                        if let Some(e) = state.workspaces.get_mut(&rws.workspace_id) {
                            e.last_remote_label = Some(new_remote);
                        }
                    }
                    LabelAction::RestampLocal => {
                        deps.local
                            .request("workspace.rename", json!({ "workspace_id": entry.local_id, "label": label }))
                            .await?;
                        if let Some(e) = state.workspaces.get_mut(&rws.workspace_id) {
                            e.last_remote_label = Some(rws.label.clone());
                        }
                    }
                    LabelAction::InSync => {
                        if entry.last_remote_label.as_deref() != Some(rws.label.as_str()) {
                            if let Some(e) = state.workspaces.get_mut(&rws.workspace_id) {
                                e.last_remote_label = Some(rws.label.clone());
                            }
                        }
                    }
                }
            }
        } else {
            // adopt a label-matching unmapped local workspace (orphan from a crash)
            let mapped: HashSet<&str> = state.workspaces.values().map(|e| e.local_id.as_str()).collect();
            let orphan = local_snap
                .workspaces
                .iter()
                .find(|w| w.label == label && !mapped.contains(w.workspace_id.as_str()));
            let entry = if let Some(orphan) = orphan {
                log.log(&format!("adopting existing workspace {label} ({})", orphan.workspace_id));
                WsEntry {
                    local_id: orphan.workspace_id.clone(),
                    tombstone: None,
                    root_tab_local_id: if orphan.tab_count == Some(1) && orphan.pane_count == Some(1) {
                        orphan.active_tab_id.clone()
                    } else {
                        None
                    },
                    last_remote_label: Some(rws.label.clone()),
                }
            } else {
                log.log(&format!("creating mirror workspace {label}"));
                #[derive(Deserialize)]
                struct Created {
                    workspace: CreatedWs,
                    tab: CreatedTab,
                }
                #[derive(Deserialize)]
                struct CreatedWs {
                    workspace_id: String,
                }
                #[derive(Deserialize)]
                struct CreatedTab {
                    tab_id: String,
                }
                // same non-git marker cwd the mirror panes use, so the
                // workspace's default pane never flashes a (misleading) sidebar
                // git branch before layout.apply swaps in the real mirror panes
                let cwd = mirror_pane_cwd(&deps.state_dir).display().to_string();
                let created: Created = deps
                    .local
                    .request_t("workspace.create", json!({ "label": label, "cwd": cwd, "focus": false }))
                    .await?;
                WsEntry {
                    local_id: created.workspace.workspace_id,
                    tombstone: None,
                    root_tab_local_id: Some(created.tab.tab_id),
                    last_remote_label: Some(rws.label.clone()),
                }
            };
            local_ws_ids.insert(entry.local_id.clone());
            state.workspaces.insert(rws.workspace_id.clone(), entry);
        }
    }

    // 3b. forward the remote's workspace tokens onto the mirror rows, so a mirror
    //     carries the same values a native workspace does under whatever layout is
    //     configured locally. Ignored by a pre-0.7.4 local server.
    let source = mirror_source(&host.name);
    for rws in &remote_snap.workspaces {
        if rws.tokens.is_empty() {
            continue; // nothing to forward (also the pre-0.7.4 remote case)
        }
        let Some(entry) = state.workspaces.get(&rws.workspace_id) else { continue };
        if entry.is_tombstoned() || !local_ws_ids.contains(&entry.local_id) {
            continue;
        }
        let _ = deps
            .local
            .request(
                "workspace.report_metadata",
                json!({ "workspace_id": entry.local_id, "source": source, "tokens": rws.tokens }),
            )
            .await;
    }

    // 4. remote tabs → replicate layout with wrapper commands
    for rtab in &remote_snap.tabs {
        let Some(ws_entry) = state.workspaces.get(&rtab.workspace_id).cloned() else { continue };
        if ws_entry.is_tombstoned() {
            continue;
        }
        let tab_entry = state.tabs.get(&rtab.tab_id).cloned();
        let tab_exists = tab_entry.as_ref().is_some_and(|t| local_tab_ids.contains(t.local_id.as_str()));
        let remote_panes_in_tab: Vec<&PaneInfo> =
            remote_snap.panes.iter().filter(|p| p.tab_id == rtab.tab_id).collect();

        if !tab_exists || remote_panes_in_tab.iter().any(|p| !state.panes.contains_key(&p.pane_id)) {
            #[derive(Deserialize)]
            struct Exported {
                layout: ExportedLayout,
            }
            #[derive(Deserialize)]
            struct ExportedLayout {
                root: LayoutNode,
            }
            let exported: Exported =
                deps.remote.request_t("layout.export", json!({ "tab_id": rtab.tab_id })).await?;
            let mut remote_order = Vec::new();
            walk_pane_ids(&exported.layout.root, &mut remote_order);

            if !tab_exists {
                // non-git cwd so herdr shows no (misleading) sidebar git status
                // for the mirror; the pane exec's the streamer regardless
                let cwd = mirror_pane_cwd(&deps.state_dir).display().to_string();
                let root = map_node(&exported.layout.root, &cwd);
                let target_tab = ws_entry.root_tab_local_id.clone();
                // tab_id and workspace_id are mutually exclusive on layout.apply
                let mut params = json!({ "tab_label": rtab.label, "root": root, "focus": false });
                match &target_tab {
                    Some(t) => params["tab_id"] = json!(t),
                    None => params["workspace_id"] = json!(ws_entry.local_id),
                }
                #[derive(Deserialize)]
                struct Applied {
                    layout: AppliedLayout,
                }
                #[derive(Deserialize)]
                struct AppliedLayout {
                    tab_id: String,
                    root: LayoutNode,
                }
                let applied: Applied = deps.local.request_t("layout.apply", params).await?;
                // consume the root tab only AFTER a successful apply, so a
                // transient failure retries against it instead of stacking a tab
                if let Some(ws) = state.workspaces.get_mut(&rtab.workspace_id) {
                    ws.root_tab_local_id = None;
                }
                state
                    .tabs
                    .insert(rtab.tab_id.clone(), crate::state::TabEntry { local_id: applied.layout.tab_id });
                let mut local_order = Vec::new();
                walk_pane_ids(&applied.layout.root, &mut local_order);
                for (i, rid) in remote_order.iter().enumerate() {
                    if rid.is_empty() || local_order.get(i).is_none_or(|l| l.is_empty()) {
                        continue;
                    }
                    if state.panes.get(rid).is_some_and(|e| e.is_tombstoned()) {
                        continue;
                    }
                    let local_id = local_order[i].clone();
                    let seq = state.panes.get(rid).map(|e| e.seq).unwrap_or(0);
                    state.panes.insert(
                        rid.clone(),
                        PaneEntry { local_id: local_id.clone(), tombstone: None, seq, reported: None },
                    );
                    // plain pane created above; exec the streamer into it
                    spawn_streamer_pane(&deps.local, &local_id, &cmd_for(rid), &deps.log).await;
                }
            } else {
                // tab exists — add mirrors for individual new remote panes as
                // PLAIN split panes (not agent.start), then exec the streamer in.
                // agent.start would set launch_argv and surface every plain
                // terminal as a phantom "mirror" agent row.
                // non-git cwd so herdr shows no (misleading) sidebar git status
                // for the mirror; the pane exec's the streamer regardless
                let cwd = mirror_pane_cwd(&deps.state_dir).display().to_string();
                for rp in &remote_panes_in_tab {
                    if state.panes.contains_key(&rp.pane_id) {
                        continue;
                    }
                    // place the mirror where the REMOTE layout says the pane
                    // lives: split its nearest already-mirrored layout sibling
                    // in the tree's recorded direction. Falls back to the old
                    // first-pane/right when the layout doesn't resolve.
                    let placed = locate_in_layout(&exported.layout.root, &rp.pane_id).and_then(
                        |(dir, sibs)| {
                            sibs.iter()
                                .find_map(|rid| state.panes.get(rid).map(|e| e.local_id.clone()))
                                .map(|t| (t, dir))
                        },
                    );
                    let Some((target, direction)) = placed.or_else(|| {
                        remote_panes_in_tab
                            .iter()
                            .find_map(|p| state.panes.get(&p.pane_id).map(|e| e.local_id.clone()))
                            .map(|t| (t, "right".to_string()))
                    }) else {
                        continue;
                    };
                    #[derive(Deserialize)]
                    struct Split {
                        pane: SplitPane,
                    }
                    #[derive(Deserialize)]
                    struct SplitPane {
                        pane_id: String,
                    }
                    let split: Split = deps
                        .local
                        .request_t(
                            "pane.split",
                            json!({
                                "target_pane_id": target,
                                "direction": direction,
                                "cwd": cwd,
                                "focus": false,
                            }),
                        )
                        .await?;
                    spawn_streamer_pane(&deps.local, &split.pane.pane_id, &cmd_for(&rp.pane_id), &deps.log).await;
                    state.panes.insert(
                        rp.pane_id.clone(),
                        PaneEntry { local_id: split.pane.pane_id, tombstone: None, seq: 0, reported: None },
                    );
                }
            }
        }

        if tab_exists {
            let tab_local = &tab_entry.as_ref().unwrap().local_id;
            let local_tab = local_snap.tabs.iter().find(|t| &t.tab_id == tab_local);
            if local_tab.is_some_and(|t| t.label != rtab.label) {
                let _ = deps
                    .local
                    .request("tab.rename", json!({ "tab_id": tab_local, "label": rtab.label }))
                    .await;
            }
        }
    }

    // 5. push authoritative agent status onto mirror panes
    push_statuses(deps, &remote_snap, state).await;
    Ok(())
}

/// Push one pane's authoritative status (or retract it when the remote agent
/// is gone). Mutates only its own entry (seq/reported). Reused by both the
/// full converge and the daemon's status fast-path.
pub async fn push_pane_status(
    local: &ApiClient,
    host_name: &str,
    remote_id: &str,
    entry: &mut PaneEntry,
    agent: Option<&AgentInfo>,
    log: &Logger,
) {
    if entry.is_tombstoned() {
        return;
    }
    let source = mirror_source(host_name);
    match agent {
        Some(agent) => {
            entry.seq += 1;
            let display = agent.display_agent.clone().or_else(|| agent.agent.clone());
            // Identity is the remote's CANONICAL id ("claude"), not the pretty
            // name: herdr canonicalizes a reported label, so this resolves the
            // real agent and the mirror row inherits its rows_by_agent layout and
            // icon instead of rendering as a nameless custom agent. The pretty
            // name still goes out below as display_agent, which is what the
            // sidebar actually shows.
            let label = agent
                .agent
                .clone()
                .or_else(|| display.clone())
                .unwrap_or_else(|| "agent".into());
            // pass through only a custom status the remote actually reports;
            // no synthetic "@host" marker (clear any stale one)
            let custom: Option<String> = agent.custom_status.as_deref().map(clamp_status);
            let status = agent.agent_status.as_deref().unwrap_or("unknown");
            let mut report = json!({
                "pane_id": entry.local_id,
                "source": source,
                "agent": label,
                "state": map_status(status),
                "seq": entry.seq,
            });
            if let Some(c) = &custom {
                report["custom_status"] = json!(c);
            }
            if let Err(e) = local.request("pane.report_agent", report).await {
                log.log(&format!("report_agent {}: {e}", entry.local_id));
            }
            // forward the remote's own tokens so a mirrored agent row carries the
            // same values a native one does, under whatever layout is configured
            // locally. Ignored by a pre-0.7.4 local server (no deny_unknown_fields).
            let mut meta = json!({
                "pane_id": entry.local_id,
                "source": source,
                "display_agent": display,
                "title": agent.name,
                "state_labels": agent.state_labels.clone().unwrap_or_default(),
                "tokens": agent.tokens.clone(),
                "seq": entry.seq,
            });
            if custom.is_none() {
                meta["clear_custom_status"] = json!(true);
            }
            let _ = local.request("pane.report_metadata", meta).await;
            entry.reported = Some(label);
        }
        None => {
            let Some(reported) = entry.reported.clone() else { return };
            // remote agent exited — retract our claim so the mirror pane doesn't
            // show a phantom agent row forever
            entry.seq += 1;
            log.log(&format!("remote agent gone on {remote_id} — releasing {reported} from {}", entry.local_id));
            if let Err(e) = local
                .request(
                    "pane.release_agent",
                    json!({ "pane_id": entry.local_id, "source": source, "agent": reported, "seq": entry.seq }),
                )
                .await
            {
                log.log(&format!("release_agent {}: {e}", entry.local_id));
            }
            entry.seq += 1;
            let _ = local
                .request(
                    "pane.report_metadata",
                    json!({
                        "pane_id": entry.local_id,
                        "source": source,
                        "clear_display_agent": true,
                        "clear_custom_status": true,
                        "clear_state_labels": true,
                        "clear_title": true,
                        "seq": entry.seq,
                    }),
                )
                .await;
            entry.reported = None;
        }
    }
}

/// Authoritative close path: apply explicit remote `*.closed` events by closing
/// the matching local mirror and pruning state. Ids are namespaced (ws `w1`, tab
/// `w1:t1`, pane `w1:p1`), so each is looked up wherever it lives. Closing a
/// workspace mirror cascades to its tabs/panes locally; stale child state entries
/// are pruned by the next converge.
pub async fn apply_remote_closes(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host_name: &str,
    closed: &[String],
    log: &Logger,
) {
    if closed.is_empty() {
        return;
    }
    let mut state = load_state(state_dir, host_name);
    let mut changed = false;
    for rid in closed {
        if let Some(entry) = state.workspaces.remove(rid) {
            changed = true;
            if !entry.is_tombstoned() {
                log.log(&format!("remote workspace {rid} closed — closing mirror {}", entry.local_id));
                let _ = local.request("workspace.close", json!({ "workspace_id": entry.local_id })).await;
            }
        } else if let Some(entry) = state.tabs.remove(rid) {
            changed = true;
            let _ = local.request("tab.close", json!({ "tab_id": entry.local_id })).await;
        } else if let Some(entry) = state.panes.remove(rid) {
            changed = true;
            if !entry.is_tombstoned() {
                let _ = local.request("pane.close", json!({ "pane_id": entry.local_id })).await;
            }
        }
    }
    if changed {
        if let Err(e) = save_state(state_dir, host_name, &state) {
            log.log(&format!("[{host_name}] state save failed: {e}"));
        }
    }
}

pub async fn push_statuses(deps: &ConvergeDeps, remote_snap: &Snapshot, state: &mut HostState) {
    let agent_by_pane: HashMap<&str, &AgentInfo> =
        remote_snap.agents.iter().map(|a| (a.pane_id.as_str(), a)).collect();
    for (remote_id, entry) in state.panes.iter_mut() {
        let agent = agent_by_pane.get(remote_id.as_str()).copied();
        push_pane_status(&deps.local, &deps.host.name, remote_id, entry, agent, &deps.log).await;
    }
}

/// Mark mirrored agents unknown (ssh drop) — statuses recover on reconnect.
/// Only panes we actually reported an agent onto; inventing agent rows for
/// plain mirrored terminals pollutes the agents panel.
pub async fn mark_unknown(local: &ApiClient, state_dir: &std::path::Path, host_name: &str, reason: &str) {
    let mut state = load_state(state_dir, host_name);
    let source = mirror_source(host_name);
    let custom = clamp_status(reason);
    for entry in state.panes.values_mut() {
        let Some(reported) = entry.reported.clone() else { continue };
        if entry.is_tombstoned() {
            continue;
        }
        entry.seq += 1;
        let _ = local
            .request(
                "pane.report_agent",
                json!({
                    "pane_id": entry.local_id,
                    "source": source,
                    "agent": reported,
                    "state": "unknown",
                    "custom_status": custom,
                    "seq": entry.seq,
                }),
            )
            .await;
    }
    let _ = save_state(state_dir, host_name, &state);
}

/// Graceful teardown: close every mirror workspace this host created.
pub async fn teardown(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host_name: &str,
    log: &Logger,
    closes: Option<&crate::closes::Closes>,
) -> Result<()> {
    let state = load_state(state_dir, host_name);
    // Wipe the id map BEFORE closing the local windows. teardown (and the
    // restart / zombie-heal that call it) means "stop mirroring here" — never
    // "close the remote sessions". But close_remote_on_local_close fires when a
    // converge sees a still-mapped mirror vanish locally, and it can't tell our
    // bulk close from the user pressing prefix-x. Clearing the map first leaves
    // nothing to attribute these closes to, so they cannot propagate to the
    // remote. Manual close is unaffected: there the entry is still mapped when
    // the user closes it, so the intent still reaches the remote.
    save_state(state_dir, host_name, &HostState::default())?;
    for entry in state.workspaces.values() {
        log.log(&format!("closing mirror workspace {}", entry.local_id));
        // ours, not the user's: the heal re-adopts these ids, so without the mark
        // the echoing close event would later read as "user closed the mirror"
        if let Some(c) = closes {
            if let Ok(mut t) = c.lock() {
                t.mark_self_close(&entry.local_id);
            }
        }
        let _ = local.request("workspace.close", json!({ "workspace_id": entry.local_id })).await;
    }
    Ok(())
}

async fn move_ws(local: &ApiClient, ws: &str, insert_index: usize) -> bool {
    local
        .request("workspace.move", json!({ "workspace_id": ws, "insert_index": insert_index }))
        .await
        .is_ok()
}

/// rank a workspace by its label: local (no `<prefix>: `) sorts first (0), then
/// each host's mirrors by config order (i+1). First matching prefix wins.
fn ws_rank(label: &str, prefixes: &[String]) -> usize {
    for (i, p) in prefixes.iter().enumerate() {
        if label.starts_with(&format!("{p}: ")) {
            return i + 1;
        }
    }
    0
}

/// Pure planner: given the current `(workspace_id, rank)` order, return the
/// `(workspace_id, insert_index)` workspace.move calls that group the sidebar
/// (locals first, then mirror ranks ascending, preserving order within each
/// group), moving ONLY mirror rows (rank > 0). Empty when already grouped.
///
/// `insert_index` is herdr's pre-removal gap index: pulling a row up lands it at
/// `i`; pushing one to the end uses `insert_index == len`.
fn plan_regroup(current: &[(String, usize)]) -> Vec<(String, usize)> {
    let mut target = current.to_vec();
    target.sort_by_key(|(_, r)| *r); // stable: preserves order within each group
    if current == target.as_slice() {
        return Vec::new();
    }
    let mut moves = Vec::new();
    let mut working = current.to_vec();
    let n = working.len();
    let mut i = 0usize;
    let mut guard = 0usize;
    while i < target.len() {
        guard += 1;
        if guard > n * n + 8 {
            break;
        }
        if working[i].0 == target[i].0 {
            i += 1;
            continue;
        }
        if target[i].1 > 0 {
            // a mirror belongs at i and is currently later — pull it up to i
            let want = target[i].0.clone();
            let src = working.iter().position(|(id, _)| *id == want).unwrap();
            moves.push((want.clone(), i));
            let item = working.remove(src);
            working.insert(i, item);
            i += 1;
        } else if i + 1 < working.len() {
            // a local belongs at i but a mirror sits there — push that mirror to the end
            let m = working[i].0.clone();
            moves.push((m.clone(), working.len()));
            let item = working.remove(i);
            working.push(item);
        } else {
            i += 1;
        }
    }
    moves
}

/// Keep the local sidebar grouped: local (non-mirror) workspaces first, then each
/// host's mirror workspaces contiguous in config order. Classifies by the
/// `<prefix>: ` label the mirror sets, and only ever moves mirror rows — local
/// workspaces are never reordered (they group as a side effect of mirror rows
/// being pushed below them). Idempotent: issues no moves when already grouped.
pub async fn regroup_sidebar(local: &ApiClient, prefixes: &[String], log: &Logger) {
    let Ok(snap) = fetch_snapshot(local).await else { return };
    let current: Vec<(String, usize)> =
        snap.workspaces.iter().map(|w| (w.workspace_id.clone(), ws_rank(&w.label, prefixes))).collect();
    for (ws, insert_index) in plan_regroup(&current) {
        if !move_ws(local, &ws, insert_index).await {
            log.log(&format!("regroup: move {ws} failed"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_host() -> HostConfig {
        HostConfig {
            name: "vps".into(),
            target: "vps".into(),
            kind: crate::config::HostKind::Ssh,
            docker_bin: "docker".into(),
            prefix: "vps".into(),
            remote_bin: "~/.local/bin/herdr".into(),
            always_control: true,
        }
    }

    /// Characterization test: the ssh pane argv is a cross-process contract.
    ///
    /// The daemon spawns `herdr-mirror pane ...` as a separate process, and
    /// `count_streamers` (daemon.rs) identifies a host's live streamers by
    /// string-matching `--ctl-path` in that argv. Nothing else pins the shape,
    /// so a change here silently breaks mirror healing on upgrade: streamers
    /// started by the old binary carry the old argv, the new daemon fails to
    /// match them, concludes they died, and re-execs over live panes.
    ///
    /// If this test fails, that is the question to answer — not a prompt to
    /// update the expected value.
    #[test]
    fn ssh_pane_argv_is_stable() {
        let state_dir = std::path::Path::new("/state");
        let cmd = cmd_for_pane(&ssh_host(), state_dir, &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            [
                "pane",
                "vps",
                "w1:p1",
                "--remote-bin",
                "~/.local/bin/herdr",
                "--always-control",
                "--ctl-path",
                "/state/vps.ctl",
            ]
        );
        // argv[0] is the resolved exe path, which varies by install
        assert!(argv[0].ends_with("herdr-mirror") || argv[0].contains("herdr_mirror"), "{}", argv[0]);
    }

    /// Docker hosts append their flags *after* the ssh-shaped prefix, so the
    /// two argv layouts share a stable head and only diverge at the tail.
    #[test]
    fn docker_pane_argv_carries_container_and_no_identity_token() {
        let mut host = ssh_host();
        host.name = "token".into();
        host.target = "/Users/n/proj".into();
        host.kind = crate::config::HostKind::DockerFolder("/Users/n/proj".into());
        host.docker_bin = "/usr/local/bin/docker".into();
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            [
                "pane",
                "/Users/n/proj",
                "w1:p1",
                "--remote-bin",
                "~/.local/bin/herdr",
                "--always-control",
                // no identity token at all: healing asks herdr per pane
                "--container-folder",
                "/Users/n/proj",
                "--docker-bin",
                "/usr/local/bin/docker",
            ]
        );
    }

    /// The argv the daemon emits must round-trip through the pane process's
    /// own parser — they are separate processes, so nothing else checks this.
    #[test]
    fn docker_argv_round_trips_through_pane_parser() {
        let mut host = ssh_host();
        host.kind = crate::config::HostKind::DockerContainer("crazy_ride".into());
        // deliberately NOT the default "docker": parse_args defaults to the
        // same value, so a fixture using the default would still pass if
        // cmd_for_pane stopped emitting --docker-bin. Users who need an
        // absolute path (GUI-launched daemons without /usr/local/bin on PATH)
        // would then silently get "cannot run docker".
        host.docker_bin = "/usr/local/bin/docker".into();
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        let parsed = crate::pane::parse_args(&argv[2..]).expect("pane must parse daemon argv");
        assert_eq!(parsed.pane_target, "w1:p1");
        assert_eq!(parsed.ctl_path, None, "docker panes carry no ctl path");
        let ct = parsed.container.expect("container must survive the argv round trip");
        assert_eq!(ct.kind, crate::config::HostKind::DockerContainer("crazy_ride".into()));
        assert_eq!(ct.docker_bin, "/usr/local/bin/docker", "--docker-bin must round-trip");
    }

    /// always_control is the only conditional flag; its absence must not
    /// disturb the position of --ctl-path.
    #[test]
    fn ssh_pane_argv_without_always_control() {
        let mut host = ssh_host();
        host.always_control = false;
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            ["pane", "vps", "w1:p1", "--remote-bin", "~/.local/bin/herdr", "--ctl-path", "/state/vps.ctl"]
        );
    }

    #[test]
    fn ws_label_two_way_rename() {
        // in sync → nothing
        assert_eq!(resolve_ws_label("pm", "scratch", "pm: scratch", Some("scratch")), LabelAction::InSync);
        // remote renamed (history differs) → remote wins
        assert_eq!(resolve_ws_label("pm", "runs", "pm: scratch", Some("scratch")), LabelAction::RestampLocal);
        // no history (pre-upgrade state file) → remote wins once
        assert_eq!(resolve_ws_label("pm", "scratch", "pm: LLMs", None), LabelAction::RestampLocal);
        // user renamed locally, kept the prefix → push stripped name to remote
        assert_eq!(
            resolve_ws_label("pm", "scratch", "pm: LLMs", Some("scratch")),
            LabelAction::PushRemote("LLMs".into())
        );
        // user renamed locally without prefix → push as-is
        assert_eq!(
            resolve_ws_label("pm", "scratch", "LLM runs", Some("scratch")),
            LabelAction::PushRemote("LLM runs".into())
        );
        // degenerate: renamed to just the prefix-colon or whitespace → restamp
        assert_eq!(resolve_ws_label("pm", "scratch", "pm:  ", Some("scratch")), LabelAction::RestampLocal);
    }

    /// The `pane_agent_status_changed` event (herdr app/api.rs) must deserialize
    /// into AgentInfo cleanly, or flush_status would fall back to a default (no
    /// agent) and wrongly retract the mirror's agent. Note the event carries the
    /// title as `title`; the snapshot uses `name` — the alias bridges them.
    #[test]
    fn agent_status_event_parses_and_keeps_title() {
        let data = json!({
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "agent_status": "working",
            "agent": "claude",
            "title": "fix the bug",
            "display_agent": "Claude",
            "custom_status": null,
            "state_labels": { "branch": "main" }
        });
        let info: AgentInfo = serde_json::from_value(data).unwrap();
        assert_eq!(info.agent.as_deref(), Some("claude"));
        assert_eq!(info.agent_status.as_deref(), Some("working"));
        assert_eq!(info.display_agent.as_deref(), Some("Claude"));
        assert_eq!(info.name.as_deref(), Some("fix the bug")); // title -> name
        assert!(info.has_agent());
    }

    // simulate herdr's move_workspace(source, insert_index) on an id list
    fn apply_move(order: &mut Vec<String>, ws: &str, insert_index: usize) {
        let src = order.iter().position(|w| w == ws).unwrap();
        let target_idx = if src < insert_index { insert_index - 1 } else { insert_index };
        let item = order.remove(src);
        order.insert(target_idx, item);
    }

    fn ranked(items: &[(&str, usize)]) -> Vec<(String, usize)> {
        items.iter().map(|(s, r)| (s.to_string(), *r)).collect()
    }

    #[test]
    fn regroup_groups_and_only_moves_mirrors() {
        // rank 0 = local, 1 = work, 2 = vps; interleaved current order
        let current = ranked(&[("L1", 0), ("W1", 1), ("V1", 2), ("L2", 0), ("W2", 1)]);
        let moves = plan_regroup(&current);
        // never move a local
        let rank_of = |id: &str| current.iter().find(|(i, _)| i == id).unwrap().1;
        for (id, _) in &moves {
            assert!(rank_of(id) > 0, "planner moved a local row: {id}");
        }
        // applying the plan yields the grouped order
        let mut order: Vec<String> = current.iter().map(|(id, _)| id.clone()).collect();
        for (ws, idx) in &moves {
            apply_move(&mut order, ws, *idx);
        }
        assert_eq!(order, vec!["L1", "L2", "W1", "W2", "V1"]);
    }

    #[test]
    fn regroup_is_noop_when_already_grouped() {
        let current = ranked(&[("L1", 0), ("L2", 0), ("W1", 1), ("W2", 1), ("V1", 2)]);
        assert!(plan_regroup(&current).is_empty());
    }

    #[test]
    fn regroup_new_mirror_slots_into_its_block() {
        // a new work workspace appended at the bottom (the reported bug)
        let current = ranked(&[("L1", 0), ("W1", 1), ("V1", 2), ("W2", 1)]);
        let mut order: Vec<String> = current.iter().map(|(id, _)| id.clone()).collect();
        for (ws, idx) in plan_regroup(&current) {
            apply_move(&mut order, &ws, idx);
        }
        assert_eq!(order, vec!["L1", "W1", "W2", "V1"]); // W2 rises above V1
    }

    #[test]
    fn ws_rank_classifies_by_prefix() {
        let prefixes = vec!["work".to_string(), "vps".to_string()];
        assert_eq!(ws_rank("work: slice", &prefixes), 1);
        assert_eq!(ws_rank("vps: ~", &prefixes), 2);
        assert_eq!(ws_rank("utopia", &prefixes), 0); // local
    }

    /// An agent-exit event carries no agent + "unknown" status → has_agent()
    /// false, so push_pane_status retracts (the intended release path).
    #[test]
    fn agent_exit_event_reads_as_no_agent() {
        let data = json!({
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "agent_status": "unknown",
            "agent": null,
            "display_agent": null,
            "custom_status": null,
            "state_labels": null
        });
        let info: AgentInfo = serde_json::from_value(data).unwrap();
        assert!(!info.has_agent());
    }
}
