// Remote-create plugin actions: create workspaces/tabs/panes on the REMOTE
// herdr from the local one, inheriting target host and cwd from where the
// action was invoked — the same inheritance rule as native prefix+shift+n,
// extended across the machine boundary.
//
//   herdr-mirror remote-workspace           # new workspace on the context's host
//   herdr-mirror remote-tab                 # new tab in the mirrored remote workspace
//   herdr-mirror remote-split right|down    # split the mirrored remote pane
//   herdr-mirror remote-invoke <plugin>.<action>  # invoke a plugin action on the remote
//
// Resolution: the invocation context's local workspace/tab/pane ids are
// reverse-looked-up in the per-host id maps. Inside a mirror, that pins both
// the host and the remote object (and the remote pane's own cwd).
//
// Outside a mirror every action degrades instead of failing, so one key can
// cover both worlds: `remote-workspace` targets hosts.toml `default_host`
// (else the first host declared), while `remote-tab` and `remote-split` do the
// plain local thing, letting them replace native new_tab/split wholesale.
//
// The degrade is gated on NO host matching, not on the pane. Inside a mirror
// workspace whose focused pane isn't mirrored, `remote-split` still errors
// rather than splitting locally: layout_sync assumes the local tree is the
// remote tree with unmirrored panes pruned, so a local pane inside a mirrored
// tab would silently mis-place later mirrors and skew ratio sync.
//
// Anything created inside a mirror is a REMOTE object; the daemon mirrors it
// back within a couple of seconds. Local mirror objects stay daemon-owned.
//
// `remote-invoke` generalizes the trio to plugins the local herdr can't see:
// keys bound locally never reach a mirror pane's stdin, so a remote plugin's
// bindings are unreachable by typing. Instead, bind a local key to
// `remote-invoke <plugin>.<action>` and it calls `plugin.action.invoke` on the
// mirrored host with the invocation context translated to the REMOTE ids.
// Outside a mirror it invokes the same action on the local herdr — the same
// one-key-for-both-worlds degrade as remote-tab/split. The action runs on
// whichever end is chosen, so the plugin must be installed there; anything it
// does to the remote layout mirrors back like any other remote change.

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

/// cwd for a local fallback, inherited the way native new_tab/split do: shell
/// bindings carry it directly, plugin actions don't, so fall back to the
/// focused pane's cwd from the local snapshot.
async fn local_cwd(
    api: &crate::api::ApiClient,
    ctx: &InvocationContext,
) -> Result<Option<String>> {
    if let Some(cwd) = std::env::var("HERDR_ACTIVE_PANE_CWD").ok().filter(|s| !s.is_empty()) {
        return Ok(Some(cwd));
    }
    let Some(pane_id) = &ctx.focused_pane_id else { return Ok(None) };
    let snap = fetch_snapshot(api).await?;
    Ok(snap
        .panes
        .iter()
        .find(|p| &p.pane_id == pane_id)
        .and_then(|p| p.foreground_cwd.clone().or_else(|| p.cwd.clone())))
}

/// The outside-a-mirror fallback for `remote-tab`: a plain local tab in the
/// invocation workspace, matching native new_tab (cwd inherited from the
/// focused pane, focused).
async fn local_tab(env: &Env, ctx: &InvocationContext) -> Result<()> {
    let api = crate::api::ApiClient::connect(&env.local_socket).await?;
    let cwd = local_cwd(&api, ctx).await?;
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

/// The outside-a-mirror fallback for `remote-split`: split the focused local
/// pane, matching native split_vertical/horizontal. Only ever reached when NO
/// host matched — inside a mirror an unmirrored focused pane still errors,
/// because a local pane in a mirrored tab would break the invariant
/// layout_sync depends on (see the module header).
async fn local_split(env: &Env, ctx: &InvocationContext, direction: &str) -> Result<()> {
    let api = crate::api::ApiClient::connect(&env.local_socket).await?;
    let cwd = local_cwd(&api, ctx).await?;
    let mut params = json!({ "direction": direction, "cwd": cwd, "focus": true });
    // no focused pane in context: let herdr split whatever is focused
    if let Some(pane_id) = &ctx.focused_pane_id {
        params["target_pane_id"] = json!(pane_id);
    }
    let res: Value = api.request("pane.split", params).await?;
    println!(
        "split local pane {} {direction} → {}",
        ctx.focused_pane_id.as_deref().unwrap_or("focused"),
        res.pointer("/pane/pane_id").and_then(|v| v.as_str()).unwrap_or("?")
    );
    Ok(())
}

/// Context for the local-invoke fallback: the plugin-action JSON verbatim when
/// present (full fidelity — cwd, tab, selection all pass through), else the
/// `HERDR_ACTIVE_*` fields a shell binding gets.
fn local_context_value(ctx: &InvocationContext) -> Value {
    if let Some(v) = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
    {
        return v;
    }
    let mut out = json!({});
    if let Some(ws) = &ctx.workspace_id {
        out["workspace_id"] = json!(ws);
    }
    if let Some(pane) = &ctx.focused_pane_id {
        out["focused_pane_id"] = json!(pane);
    }
    if let Some(cwd) = std::env::var("HERDR_ACTIVE_PANE_CWD").ok().filter(|s| !s.is_empty()) {
        out["focused_pane_cwd"] = json!(cwd);
    }
    out
}

/// `remote-invoke <plugin>.<action>`: run a plugin action on the mirrored
/// host, with the invocation context translated to the remote ids. Outside a
/// mirror the action is invoked on the local herdr instead. A bare action id
/// (no dot) is passed without a plugin_id for the server to resolve.
pub async fn invoke(env: Env, spec: &str) -> Result<()> {
    let (plugin_id, action_id) = match spec.split_once('.') {
        Some((p, a)) if !p.is_empty() && !a.is_empty() => (Some(p), a),
        Some(_) => return Err(err(format!("bad action spec {spec:?}: want <plugin>.<action>"))),
        None => (None, spec),
    };

    let ctx = invocation_context();
    // Same deliberate non-`?` as run(): the local fallback needs no host config.
    let config = load_config(&env.config_search);
    let resolved = config.as_ref().ok().and_then(|c| resolve_context(&env, &c.hosts, &ctx));

    let Some(resolved) = resolved else {
        let api = crate::api::ApiClient::connect(&env.local_socket).await?;
        let mut params = json!({ "action_id": action_id, "context": local_context_value(&ctx) });
        if let Some(p) = plugin_id {
            params["plugin_id"] = json!(p);
        }
        api.request("plugin.action.invoke", params).await?;
        println!("invoked {spec} locally");
        return Ok(());
    };

    let mut remote = RemoteHost::new(&resolved.host, &env.state_dir);
    let (api, _status) = remote.connect_api().await?;

    // the remote action sees the REMOTE objects behind the invoking mirror,
    // including the real cwd of the pane it will act on
    let mut context = json!({ "invocation_source": "mirror" });
    if let Some(ws) = &resolved.remote_ws_id {
        context["workspace_id"] = json!(ws);
    }
    if let Some(pane_id) = &resolved.remote_pane_id {
        context["focused_pane_id"] = json!(pane_id);
        let snap = fetch_snapshot(&api).await?;
        if let Some(pane) = snap.panes.iter().find(|p| &p.pane_id == pane_id) {
            if let Some(cwd) = pane.foreground_cwd.clone().or_else(|| pane.cwd.clone()) {
                context["focused_pane_cwd"] = json!(cwd);
            }
        }
    }
    let mut params = json!({ "action_id": action_id, "context": context });
    if let Some(p) = plugin_id {
        params["plugin_id"] = json!(p);
    }
    api.request("plugin.action.invoke", params).await.map_err(|e| {
        err(format!(
            "remote invoke {spec} on {}: {e} (needs the plugin installed there, and a herdr with plugin.action.invoke)",
            resolved.host.name
        ))
    })?;
    println!("invoked {spec} on {}", resolved.host.name);
    Ok(())
}

pub async fn run(env: Env, kind: &str, direction: Option<&str>) -> Result<()> {
    if kind == "split" && !matches!(direction, Some("right") | Some("down")) {
        return Err(err("remote-split needs a direction: right|down"));
    }

    let ctx = invocation_context();
    // Deliberately not `?`: the local fallback below needs no host config, and
    // these actions are meant to be bound over herdr's native new_tab/split.
    // Hard-failing here would kill that key for anyone who hasn't written
    // hosts.toml yet, with an error about SSH hosts they never asked for.
    let config = load_config(&env.config_search);
    let resolved =
        config.as_ref().ok().and_then(|c| resolve_context(&env, &c.hosts, &ctx));

    // One key for both worlds: inside a mirror these take the remote path
    // below; anywhere else they degrade to the plain local action instead of
    // erroring, so they can replace native new_tab/split wholesale. Only fires
    // when NO host matched — in a mirror workspace whose focused pane isn't
    // mirrored we still go remote, never dropping a local object into a
    // daemon-owned workspace.
    if resolved.is_none() {
        match kind {
            "tab" => return local_tab(&env, &ctx).await,
            // direction was validated at the top of this fn
            "split" => return local_split(&env, &ctx, direction.unwrap()).await,
            _ => {}
        }
    }
    // past the fallback every path needs a host, so a bad config is fatal now
    let config = config?;

    if resolved.is_none() && kind != "workspace" {
        return Err(err(format!(
            "remote {kind}: invoke this from inside a mirror workspace so the target host and pane are known"
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
