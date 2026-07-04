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
    pub name: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    pub custom_status: Option<String>,
    pub state_labels: Option<BTreeMap<String, String>>,
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

fn walk_pane_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Pane { pane_id, .. } => out.push(pane_id.clone().unwrap_or_default()),
        LayoutNode::Split { first, second, .. } => {
            walk_pane_ids(first, out);
            walk_pane_ids(second, out);
        }
    }
}

/// Rebuild the layout tree as JSON with each pane running the wrapper command.
fn map_node(node: &LayoutNode, cmd_for: &dyn Fn(&str) -> Vec<String>, cwd: &str) -> Value {
    match node {
        LayoutNode::Pane { pane_id, label } => json!({
            "type": "pane",
            "label": label,
            "command": cmd_for(pane_id.as_deref().unwrap_or("")),
            "cwd": cwd,
        }),
        LayoutNode::Split { direction, ratio, first, second } => json!({
            "type": "split",
            "direction": direction,
            "ratio": ratio,
            "first": map_node(first, cmd_for, cwd),
            "second": map_node(second, cmd_for, cwd),
        }),
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

pub struct ConvergeDeps {
    pub local: ApiClient,
    pub remote: ApiClient,
    pub host: HostConfig,
    pub state_dir: PathBuf,
    pub plugin_root: PathBuf,
    pub log: Logger,
}

/// argv for one mirror pane: this same binary in `pane` mode. Panes without a
/// known size get no --cols/--rows (the wrapper falls back to a default).
fn cmd_for_pane(deps: &ConvergeDeps, sizes: &HashMap<String, LayoutRect>) -> impl Fn(&str) -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "herdr-mirror".into());
    let target = deps.host.target.clone();
    let remote_bin = deps.host.remote_bin.clone();
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
    let cmd_for = cmd_for_pane(deps, &sizes);

    // 1. detect mirrors the user closed locally → tombstone, never recreate
    for (rid, entry) in state.workspaces.iter_mut() {
        if !entry.is_tombstoned() && !local_ws_ids.contains(&entry.local_id) && remote_ws_ids.contains(rid.as_str()) {
            log.log(&format!("workspace mirror for {rid} was closed locally — tombstoning"));
            entry.tombstone = Some(true);
        }
    }
    let pane_ws: HashMap<&str, &str> =
        remote_snap.panes.iter().map(|p| (p.pane_id.as_str(), p.workspace_id.as_str())).collect();
    let mut drop_panes: Vec<String> = Vec::new();
    for (rid, entry) in state.panes.iter_mut() {
        if !entry.is_tombstoned() && !local_pane_ids.contains(entry.local_id.as_str()) && remote_pane_ids.contains(rid.as_str()) {
            let ws_entry = pane_ws.get(rid.as_str()).and_then(|ws| state.workspaces.get(*ws));
            // if the pane's whole mirror workspace is gone, the stale pane
            // entry is collateral — drop it so recreation maps cleanly
            match ws_entry {
                Some(w) if !w.is_tombstoned() && local_ws_ids.contains(&w.local_id) => {
                    log.log(&format!("pane mirror for {rid} was closed locally — tombstoning"));
                    entry.tombstone = Some(true);
                }
                _ => drop_panes.push(rid.clone()),
            }
        }
    }
    for rid in drop_panes {
        state.panes.remove(&rid);
    }

    // 2. remote objects that disappeared → close their mirrors, drop map entries
    let gone_ws: Vec<String> =
        state.workspaces.keys().filter(|rid| !remote_ws_ids.contains(rid.as_str())).cloned().collect();
    for rid in gone_ws {
        let entry = state.workspaces.remove(&rid).unwrap();
        if !entry.is_tombstoned() && local_ws_ids.contains(&entry.local_id) {
            log.log(&format!("remote workspace {rid} gone — closing mirror {}", entry.local_id));
            if let Err(e) = deps.local.request("workspace.close", json!({ "workspace_id": entry.local_id })).await {
                log.log(&format!("close failed: {e}"));
            }
        }
    }
    let gone_tabs: Vec<String> =
        state.tabs.keys().filter(|rid| !remote_tab_ids.contains(rid.as_str())).cloned().collect();
    for rid in gone_tabs {
        let entry = state.tabs.remove(&rid).unwrap();
        if local_tab_ids.contains(entry.local_id.as_str()) {
            let _ = deps.local.request("tab.close", json!({ "tab_id": entry.local_id })).await;
        }
    }
    let gone_panes: Vec<String> =
        state.panes.keys().filter(|rid| !remote_pane_ids.contains(rid.as_str())).cloned().collect();
    for rid in gone_panes {
        let entry = state.panes.remove(&rid).unwrap();
        if !entry.is_tombstoned() && local_pane_ids.contains(entry.local_id.as_str()) {
            let _ = deps.local.request("pane.close", json!({ "pane_id": entry.local_id })).await;
        }
    }

    // 3. remote workspaces → ensure mirrors exist with the right label
    for rws in &remote_snap.workspaces {
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
            if local_ws.is_some_and(|w| w.label != label) {
                deps.local
                    .request("workspace.rename", json!({ "workspace_id": entry.local_id, "label": label }))
                    .await?;
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
                let created: Created = deps
                    .local
                    .request_t("workspace.create", json!({ "label": label, "focus": false }))
                    .await?;
                WsEntry {
                    local_id: created.workspace.workspace_id,
                    tombstone: None,
                    root_tab_local_id: Some(created.tab.tab_id),
                }
            };
            local_ws_ids.insert(entry.local_id.clone());
            state.workspaces.insert(rws.workspace_id.clone(), entry);
        }
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
                let cwd = deps.plugin_root.display().to_string();
                let root = map_node(&exported.layout.root, &cmd_for, &cwd);
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
                    let seq = state.panes.get(rid).map(|e| e.seq).unwrap_or(0);
                    state.panes.insert(
                        rid.clone(),
                        PaneEntry { local_id: local_order[i].clone(), tombstone: None, seq, reported: None },
                    );
                }
            } else {
                // tab exists — only add mirrors for individual new remote panes
                let tab_local = tab_entry.as_ref().unwrap().local_id.clone();
                for rp in &remote_panes_in_tab {
                    if state.panes.contains_key(&rp.pane_id) {
                        continue;
                    }
                    #[derive(Deserialize)]
                    struct Started {
                        agent: StartedAgent,
                    }
                    #[derive(Deserialize)]
                    struct StartedAgent {
                        pane_id: String,
                    }
                    let started: Started = deps
                        .local
                        .request_t(
                            "agent.start",
                            json!({
                                "name": rp.label.clone().unwrap_or_else(|| "mirror".into()),
                                "argv": cmd_for(&rp.pane_id),
                                "workspace_id": ws_entry.local_id,
                                "tab_id": tab_local,
                                "cwd": deps.plugin_root.display().to_string(),
                                "focus": false,
                            }),
                        )
                        .await?;
                    state.panes.insert(
                        rp.pane_id.clone(),
                        PaneEntry { local_id: started.agent.pane_id, tombstone: None, seq: 0, reported: None },
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
            let label = display.clone().unwrap_or_else(|| "agent".into());
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
            let mut meta = json!({
                "pane_id": entry.local_id,
                "source": source,
                "display_agent": display,
                "title": agent.name,
                "state_labels": agent.state_labels.clone().unwrap_or_default(),
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
pub async fn teardown(local: &ApiClient, state_dir: &std::path::Path, host_name: &str, log: &Logger) -> Result<()> {
    let state = load_state(state_dir, host_name);
    for entry in state.workspaces.values() {
        log.log(&format!("closing mirror workspace {}", entry.local_id));
        let _ = local.request("workspace.close", json!({ "workspace_id": entry.local_id })).await;
    }
    save_state(state_dir, host_name, &HostState::default())
}
