// Remote-create plugin actions: create workspaces/tabs/panes on the REMOTE
// herdr from the local one, inheriting target host and cwd from where the
// action was invoked — the same inheritance rule as native prefix+shift+n,
// extended across the machine boundary.
//
//   herdr-mirror remote-workspace           # new workspace on the context's host
//   herdr-mirror remote-tab                 # new tab in the mirrored remote workspace
//   herdr-mirror remote-split right|down    # split the mirrored remote pane
//   herdr-mirror smart-tab                  # remote-tab inside a mirror, local tab elsewhere
//
// Resolution: the invocation context's local workspace/tab/pane ids are
// reverse-looked-up in the per-host id maps. Inside a mirror, that pins both
// the host and the remote object (and the remote pane's own cwd). Outside a
// mirror, only `remote-workspace` works, targeting hosts.toml `default_host`
// (else the first host declared) — and `smart-tab`, which degrades to a plain
// local tab so one key can replace native new_tab wholesale.
//
// These create REMOTE objects only; the daemon mirrors them back within a
// couple of seconds. Local mirror objects stay daemon-owned.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{load_config, HostConfig};
use crate::mirror::fetch_snapshot;
use crate::remote::RemoteHost;
use crate::state::load_state;
use crate::util::{err, Env, Result};

#[derive(Debug, Default, Deserialize)]
struct InvocationContext {
    workspace_id: Option<String>,
    focused_pane_id: Option<String>,
}

struct Resolved {
    host: HostConfig,
    remote_ws_id: Option<String>,
    remote_pane_id: Option<String>,
}

/// find which host (if any) mirrors the workspace the action was invoked from
fn resolve_context(env: &Env, hosts: &[HostConfig], ctx: &InvocationContext) -> Option<Resolved> {
    for host in hosts {
        let state = load_state(&env.state_dir, &host.name);
        let ws_hit = state.workspaces.iter().find(|(_, e)| {
            Some(&e.local_id) == ctx.workspace_id.as_ref() && !e.is_tombstoned()
        });
        let Some((ws_rid, _)) = ws_hit else { continue };
        let pane_hit = state.panes.iter().find(|(_, e)| {
            Some(&e.local_id) == ctx.focused_pane_id.as_ref() && !e.is_tombstoned()
        });
        return Some(Resolved {
            host: host.clone(),
            remote_ws_id: Some(ws_rid.clone()),
            remote_pane_id: pane_hit.map(|(rid, _)| rid.clone()),
        });
    }
    None
}

/// Invocation context from `HERDR_PLUGIN_CONTEXT_JSON` (plugin actions), with
/// the `HERDR_ACTIVE_*` variables herdr hands to `[[keys.command]]` shell
/// bindings as a fallback — so the actions work identically from either.
fn invocation_context() -> InvocationContext {
    let mut ctx: InvocationContext = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let env_id = |name: &str| std::env::var(name).ok().filter(|s| !s.is_empty());
    if ctx.workspace_id.is_none() {
        ctx.workspace_id = env_id("HERDR_ACTIVE_WORKSPACE_ID");
    }
    if ctx.focused_pane_id.is_none() {
        ctx.focused_pane_id = env_id("HERDR_ACTIVE_PANE_ID");
    }
    ctx
}

/// The smart-tab fallback: a plain local tab in the invocation workspace,
/// matching native new_tab (cwd inherited from the focused pane, focused).
async fn local_tab(env: &Env, ctx: &InvocationContext) -> Result<()> {
    let api = crate::api::ApiClient::connect(&env.local_socket).await?;
    // shell bindings carry the cwd directly; plugin actions don't, so fall
    // back to the focused pane's cwd from the local snapshot
    let mut cwd = std::env::var("HERDR_ACTIVE_PANE_CWD").ok().filter(|s| !s.is_empty());
    if cwd.is_none() {
        if let Some(pane_id) = &ctx.focused_pane_id {
            let snap = fetch_snapshot(&api).await?;
            cwd = snap
                .panes
                .iter()
                .find(|p| &p.pane_id == pane_id)
                .and_then(|p| p.foreground_cwd.clone().or_else(|| p.cwd.clone()));
        }
    }
    let mut params = json!({ "cwd": cwd, "focus": true });
    if let Some(ws) = &ctx.workspace_id {
        params["workspace_id"] = json!(ws);
    }
    let res: Value = api.request("tab.create", params).await?;
    println!(
        "created local tab {}",
        res.pointer("/tab/tab_id").and_then(|v| v.as_str()).unwrap_or("?")
    );
    Ok(())
}

pub async fn run(env: Env, kind: &str, direction: Option<&str>) -> Result<()> {
    if kind == "split" && !matches!(direction, Some("right") | Some("down")) {
        return Err(err("remote-split needs a direction: right|down"));
    }

    let ctx = invocation_context();
    let config = load_config(&env.config_search)?;
    let resolved = resolve_context(&env, &config.hosts, &ctx);

    // smart-tab: one key for both worlds. Inside a mirror workspace it is
    // exactly remote-tab; anywhere else it degrades to a plain local tab
    // instead of erroring, so it can replace native new_tab wholesale.
    if kind == "smart-tab" && resolved.is_none() {
        return local_tab(&env, &ctx).await;
    }
    let kind = if kind == "smart-tab" { "tab" } else { kind };

    if resolved.is_none() && kind != "workspace" {
        return Err(err(format!(
            "remote {kind}: invoke this from inside a mirror workspace so the target host and {} are known",
            if kind == "tab" { "workspace" } else { "pane" }
        )));
    }
    let host = resolved
        .as_ref()
        .map(|r| r.host.clone())
        .or_else(|| config.default_host().cloned())
        .ok_or_else(|| err("no hosts configured"))?;

    let mut remote = RemoteHost::new(&host, &env.state_dir);
    let (api, _status) = remote.connect_api().await?;

    // cwd inheritance comes from the REMOTE side: the remote pane behind the
    // focused mirror pane knows its real cwd; local cwds are meaningless there
    let mut cwd: Option<String> = None;
    if let Some(pane_id) = resolved.as_ref().and_then(|r| r.remote_pane_id.clone()) {
        let snap = fetch_snapshot(&api).await?;
        if let Some(pane) = snap.panes.iter().find(|p| p.pane_id == pane_id) {
            cwd = pane.foreground_cwd.clone().or_else(|| pane.cwd.clone());
        }
    }

    match kind {
        "workspace" => {
            let res: Value = api.request("workspace.create", json!({ "cwd": cwd, "focus": false })).await?;
            println!(
                "created workspace {} ({}) on {}; mirror follows shortly",
                res.pointer("/workspace/label").and_then(|v| v.as_str()).unwrap_or("?"),
                res.pointer("/workspace/workspace_id").and_then(|v| v.as_str()).unwrap_or("?"),
                host.name
            );
        }
        "tab" => {
            let ws = resolved.as_ref().and_then(|r| r.remote_ws_id.clone()).unwrap();
            let res: Value = api
                .request("tab.create", json!({ "workspace_id": ws, "cwd": cwd, "focus": false }))
                .await?;
            println!(
                "created tab {} in {}: {ws}; mirror follows shortly",
                res.pointer("/tab/tab_id").and_then(|v| v.as_str()).unwrap_or("?"),
                host.name
            );
        }
        "split" => {
            let Some(pane_id) = resolved.as_ref().and_then(|r| r.remote_pane_id.clone()) else {
                return Err(err("remote split: the focused pane is not a mirrored pane"));
            };
            let dir = direction.unwrap();
            let res: Value = api
                .request(
                    "pane.split",
                    json!({ "target_pane_id": pane_id, "direction": dir, "cwd": cwd, "focus": false }),
                )
                .await?;
            println!(
                "split {pane_id} {dir} on {} → {}; mirror follows shortly",
                host.name,
                res.pointer("/pane/pane_id").and_then(|v| v.as_str()).unwrap_or("ok")
            );
        }
        _ => return Err(err(format!("unknown remote action: {kind}"))),
    }
    Ok(())
}
