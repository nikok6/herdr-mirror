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
// Inside a mirror workspace it requires a MIRRORED focused pane, like
// remote-split: the remote backfills omitted context from its own focused
// pane, so an untranslated pane would target an arbitrary remote object.

use std::io::IsTerminal;

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

/// Entry points for main. Run the action; on failure post a toast to the
/// local herdr before propagating. Key-bound and plugin-action invocations
/// have their output discarded, so without the toast a failed keypress is
/// indistinguishable from a dead key. Interactive runs (stderr is a tty)
/// already see the error and skip it. Best effort on every leg: a herdr
/// without notification.show, or none reachable, changes nothing.
pub async fn invoke_cmd(env: Env, spec: &str) -> Result<()> {
    let what = format!("remote-invoke {spec}");
    report_failure(&env, &what, invoke(&env, spec).await).await
}

pub async fn run_cmd(env: Env, kind: &str, direction: Option<&str>) -> Result<()> {
    let what = format!("remote-{kind}");
    report_failure(&env, &what, run(&env, kind, direction).await).await
}

async fn report_failure(env: &Env, what: &str, result: Result<()>) -> Result<()> {
    let Err(e) = result else { return Ok(()) };
    if !std::io::stderr().is_terminal() {
        if let Ok(api) = crate::api::ApiClient::connect(&env.local_socket).await {
            let _ = api
                .request(
                    "notification.show",
                    json!({ "title": format!("mirror: {what} failed"), "body": e.to_string() }),
                )
                .await;
        }
    }
    Err(e)
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
async fn invoke(env: &Env, spec: &str) -> Result<()> {
    // The spec goes to the server verbatim as action_id: herdr matches it
    // against bare action ids AND qualified <plugin>.<action> ids, and plugin
    // ids may themselves contain dots (jt.command-palette), so any client-side
    // split at a dot would pick the wrong plugin. Ambiguity is the server's
    // call too — it errors asking for the qualified form.
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(err("usage: herdr-mirror remote-invoke <plugin>.<action>"));
    }

    let ctx = invocation_context();
    // Same deliberate non-`?` as run(): the local fallback needs no host config.
    let config = load_config(&env.config_search);
    let resolved = config.as_ref().ok().and_then(|c| resolve_context(env, &c.hosts, &ctx));

    let Some(resolved) = resolved else {
        let api = crate::api::ApiClient::connect(&env.local_socket).await?;
        let params = json!({ "action_id": spec, "context": local_context_value(&ctx) });
        api.request("plugin.action.invoke", params).await?;
        println!("invoked {spec} locally");
        return Ok(());
    };

    // Same guard as remote-split, for a different reason: herdr backfills any
    // context field the client omits from the REMOTE's currently active
    // workspace, so invoking without a translated pane would hand the action
    // whatever pane/cwd happens to be focused over there — possibly an
    // unrelated project. Erroring beats silently acting on the wrong pane.
    if resolved.remote_pane_id.is_none() {
        return Err(err(format!(
            "remote invoke {spec}: the focused pane is not a mirrored pane, so the remote would fall back to its own focused pane — focus a mirror pane and retry"
        )));
    }

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
    let params = json!({ "action_id": spec, "context": context });
    api.request("plugin.action.invoke", params).await.map_err(|e| {
        err(format!(
            "remote invoke {spec} on {}: {e} (needs the plugin installed there, and a herdr with plugin.action.invoke)",
            resolved.host.name
        ))
    })?;
    println!("invoked {spec} on {}", resolved.host.name);
    Ok(())
}

async fn run(env: &Env, kind: &str, direction: Option<&str>) -> Result<()> {
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
        config.as_ref().ok().and_then(|c| resolve_context(env, &c.hosts, &ctx));

    // One key for both worlds: inside a mirror these take the remote path
    // below; anywhere else they degrade to the plain local action instead of
    // erroring, so they can replace native new_tab/split wholesale. Only fires
    // when NO host matched — in a mirror workspace whose focused pane isn't
    // mirrored we still go remote, never dropping a local object into a
    // daemon-owned workspace.
    if resolved.is_none() {
        match kind {
            "tab" => return local_tab(env, &ctx).await,
            // direction was validated at the top of this fn
            "split" => return local_split(env, &ctx, direction.unwrap()).await,
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

/// Host for `hide`/`show`: an explicit name is looked up directly (same
/// unknown-host error shape as `remote-actions`); omitted, it falls back to
/// the invocation context like `remote-tab`/`remote-split` do, so a key bound
/// inside a mirror workspace hides/shows whichever connection it's pressed in.
async fn resolve_host(env: &Env, host_arg: Option<&str>) -> Result<HostConfig> {
    let config = load_config(&env.config_search)?;
    if let Some(name) = host_arg {
        return config.hosts.iter().find(|h| h.name == name).cloned().ok_or_else(|| {
            let known: Vec<&str> = config.hosts.iter().map(|h| h.name.as_str()).collect();
            err(format!(
                "unknown host {name:?} (configured: {})",
                if known.is_empty() { "none".into() } else { known.join(", ") }
            ))
        });
    }
    let ctx = invocation_context();
    resolve_context(env, &config.hosts, &ctx).map(|r| r.host).ok_or_else(|| {
        err("no host given and not invoked from inside a mirror workspace — pass one: herdr-mirror hide <host>")
    })
}

pub async fn hide_cmd(env: Env, host_arg: Option<&str>) -> Result<()> {
    report_failure(&env, "hide", hide(&env, host_arg).await).await
}

pub async fn show_cmd(env: Env, host_arg: Option<&str>) -> Result<()> {
    report_failure(&env, "show", show(&env, host_arg).await).await
}

/// Close a host's mirrors locally without touching the remote or tombstoning
/// them — reuses `mirror::teardown`'s self-close wipe, then marks the host
/// `hidden` so the daemon's next converge doesn't just recreate them.
async fn hide(env: &Env, host_arg: Option<&str>) -> Result<()> {
    let host = resolve_host(env, host_arg).await?;
    let local = crate::api::ApiClient::connect(&env.local_socket).await?;
    let log = crate::util::Logger::new(&env.state_dir, false);
    crate::mirror::teardown(&local, &env.state_dir, &host.name, &log, None).await?;
    let state = crate::state::HostState { hidden: true, ..Default::default() };
    crate::state::save_state(&env.state_dir, &host.name, &state)?;
    println!("hid {} — remote keeps running; `herdr-mirror show {}` brings it back", host.name, host.name);
    Ok(())
}

/// Clear `hidden` and nudge the daemon so its next converge recreates the
/// mirrors fresh (the old local ids are gone — `hide` closed them — so this is
/// a cold recreate, same as a first connect, not a resume).
async fn show(env: &Env, host_arg: Option<&str>) -> Result<()> {
    let host = resolve_host(env, host_arg).await?;
    let mut state = crate::state::load_state(&env.state_dir, &host.name);
    if !state.hidden {
        println!("{} is not hidden", host.name);
        return Ok(());
    }
    state.hidden = false;
    crate::state::save_state(&env.state_dir, &host.name, &state)?;
    match crate::daemon::running_pid(env) {
        Some(pid) => {
            unsafe { libc::kill(pid, libc::SIGUSR1) };
            println!("showing {} — daemon syncing now", host.name);
        }
        None => println!("showing {} — mirrors reappear when the daemon starts", host.name),
    }
    Ok(())
}

pub async fn remote_tab_for_cmd(env: Env, host_arg: &str) -> Result<()> {
    let what = format!("remote-tab-for {host_arg}");
    report_failure(&env, &what, remote_tab_for(&env, host_arg).await).await
}

/// `remote-tab-for <host>`: a new terminal on a NAMED connection, unlike
/// `remote-tab` which only knows the connection you're currently focused in
/// (via `resolve_context`). Needs the host's existing mirror workspace id —
/// there's no "focused pane" to read cwd/target from here, so this always
/// targets that workspace's default cwd rather than inheriting one.
async fn remote_tab_for(env: &Env, host_arg: &str) -> Result<()> {
    let host = resolve_host(env, Some(host_arg)).await?;
    let state = crate::state::load_state(&env.state_dir, &host.name);
    let live: Vec<(&String, &crate::state::WsEntry)> =
        state.workspaces.iter().filter(|(_, e)| !e.is_tombstoned()).collect();
    let (ws_rid, _) = match live.as_slice() {
        [] => {
            return Err(err(format!(
                "{}: no mirrored workspace yet — run `herdr-mirror start` (or `show {}` if hidden) and retry",
                host.name, host.name
            )))
        }
        [one] => *one,
        many => {
            let ids: Vec<&str> = many.iter().map(|(rid, _)| rid.as_str()).collect();
            return Err(err(format!(
                "{} mirrors {} workspaces — ambiguous which gets the new tab: {}",
                host.name,
                many.len(),
                ids.join(", ")
            )));
        }
    };

    let mut remote = RemoteHost::new(&host, &env.state_dir);
    let (api, _status) = remote.connect_api().await?;
    let res: Value =
        api.request("tab.create", json!({ "workspace_id": ws_rid, "focus": false })).await?;
    println!(
        "created tab {} in {}: {ws_rid}; mirror follows shortly",
        res.pointer("/tab/tab_id").and_then(|v| v.as_str()).unwrap_or("?"),
        host.name
    );
    Ok(())
}
