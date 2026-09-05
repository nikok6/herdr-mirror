// Host picker for new workspaces: `pick-workspace` (the plugin action) opens
// a popup plugin pane running `pick-workspace --menu`; the menu offers this
// machine plus every host in hosts.toml and creates the workspace where the
// user says.
//
// Two processes because a plugin pane is the only interactive surface a
// plugin gets: the action itself runs headless with its output discarded, so
// the summon leg only asks herdr for the popup and exits, and everything the
// user sees happens in the popup process.
//
// The summon leg forwards the invoking pane's cwd through the popup's
// environment (HERDR_MIRROR_PICK_CWD): the popup's own cwd is the plugin
// root, which would be a useless inheritance for the new local workspace.
// Remote creation passes no cwd at all, matching `remote-workspace` invoked
// outside a mirror — the remote's default is right, local paths are not.

use std::io::Write;

use serde_json::{json, Value};

use crate::api::ApiClient;
use crate::config::{load_config, HostConfig};
use crate::remote::RemoteHost;
use crate::util::{Env, Result};

const CWD_ENV: &str = "HERDR_MIRROR_PICK_CWD";

/// Plugin-action leg: open the popup that runs the menu below.
pub async fn summon(env: Env) -> Result<()> {
    let api = ApiClient::connect(&env.local_socket).await?;
    open_popup(&api, &env).await
}

async fn open_popup(api: &ApiClient, env: &Env) -> Result<()> {
    // size the popup to its content: options + title + hints + border. Width
    // tracks the longest row (name + its dim subtitle) so targets and the cwd
    // hint aren't truncated the moment they matter.
    let cwd = invoking_cwd();
    let (n_hosts, widest) = match load_config(&env.config_search) {
        Ok(c) => {
            let w = c
                .hosts
                .iter()
                .map(|h| h.name.len() + h.target.len() + 16)
                .chain(cwd.iter().map(|c| tilde(c).len() + 26))
                .max()
                .unwrap_or(0);
            (c.hosts.len(), w)
        }
        Err(_) => (0, 0),
    };
    let mut params = json!({
        "plugin_id": "mirror",
        "entrypoint": "pick-host",
        "placement": "popup",
        "width": widest.clamp(58, 90),
        // +2 for herdr's border, +1 so the hint line's trailing newline has
        // somewhere to go, +1 for the optional hosts.toml note
        "height": (n_hosts + 13).max(14),
        "focus": true,
        "env": {},
    });
    if let Some(cwd) = cwd {
        params["env"][CWD_ENV] = json!(cwd);
    }
    // herdr shows one popup at a time. If one is open, the invocation is a
    // TOGGLE: close it and stop. Reopening here instead made the key feel
    // stuck — "it's still open" — because pressing it again could never
    // dismiss the picker. If the closed popup was some other overlay, the
    // next press opens the picker; two presses beat a popup that won't die.
    if let Err(e) = api.request("plugin.pane.open", params.clone()).await {
        if e.to_string().contains("popup already open") {
            api.request("popup.close", json!({})).await?;
            return Ok(());
        }
        // Popup placement (and `popup.close`) arrived in herdr 0.7.4, and we
        // still support 0.7.2. Rather than raise the floor and lock those users
        // out of the whole plugin for one presentation detail, retry without it
        // and take herdr's default. The picker itself is identical either way.
        let mut fallback = params.clone();
        if let Some(obj) = fallback.as_object_mut() {
            obj.remove("placement");
            obj.remove("width");
            obj.remove("height");
        }
        api.request("plugin.pane.open", fallback).await?;
    }
    Ok(())
}

/// Creation hooks: turn a native create that landed in the `.mirror-pane`
/// placeholder into the thing the user actually wanted. herdr can't rebind
/// the sidebar's mouse buttons (no *_button_plugin_action exists), so native
/// creation from inside a mirror inherits the focused pane's local cwd — the
/// placeholder — and produces a bare local object nobody asked for. Each
/// event handles its own shape, so no cross-event locking is needed:
///
///   workspace.created  junk workspace (label .mirror-pane, focused, one bare
///                      pane) → close it, open the host picker
///   tab.created        junk tab in a MIRROR workspace (active tab not in the
///                      id map, one bare pane) → close it, create the tab on
///                      the remote and focus its mirror when it arrives
///   pane.created       junk split beside a mirrored pane (pane not in the
///                      map, its tab IS mapped) → close it, split the remote
///                      pane the same direction
///
/// A junk TAB also fires pane.created, but its tab is unmapped so the split
/// arm no-ops; a junk WORKSPACE also fires tab/pane.created, but it is not a
/// mirror workspace so both arms no-op. Daemon-built mirrors are excluded
/// because the daemon persists their map entry the moment each one is
/// created (see mirror::note_mapped) — the settle delay below is what gives
/// that write time to land before the map is read.
/// Is this pane the daemon's, rather than native junk?
///
/// Two independent signals, because neither alone is safe. The map is
/// authoritative but written *after* the pane exists, so a fresh mirror pane is
/// briefly unmapped. A running streamer proves ownership outright but only lands
/// once the typed `exec` has taken. Polling both for a couple of seconds turns a
/// 250ms race into one with a wide margin, and it costs nothing in the case that
/// matters: genuine native junk is never mapped and never grows a streamer, so
/// the wait always expires and we still act.
async fn settles_as_ours(
    env: &Env,
    api: &ApiClient,
    config: &crate::config::MirrorConfig,
    pane_id: &str,
) -> bool {
    for attempt in 0..8 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        for h in &config.hosts {
            let state = crate::state::load_state(&env.state_dir, &h.name);
            if state.panes.values().any(|e| e.local_id == pane_id) {
                return true;
            }
        }
        if let Ok(v) = api.request("pane.process_info", json!({ "pane_id": pane_id })).await {
            let running = v
                .pointer("/process_info/foreground_processes")
                .and_then(Value::as_array)
                .is_some_and(|procs| {
                    procs.iter().any(|p| {
                        p.get("argv").and_then(Value::as_array).is_some_and(|argv| {
                            argv.first().and_then(Value::as_str).is_some_and(|e| e.ends_with("herdr-mirror"))
                                && argv.get(1).and_then(Value::as_str) == Some("pane")
                        })
                    })
                });
            if running {
                return true;
            }
        }
    }
    false
}

/// The id of the object this event is actually about.
///
/// herdr names it for us (`HERDR_PLUGIN_EVENT_JSON`, with the context vars as a
/// fallback), so there is no need to guess. Guessing is what the first version
/// did — it rescanned the focused workspace and closed the first unmapped bare
/// placeholder pane it found — and that made every leftover pane a target for
/// every later event, including panes the user was working in.
fn event_object_id(what: &str) -> Option<String> {
    let key = match what {
        "tab" => "tab_id",
        "pane" => "pane_id",
        _ => "workspace_id",
    };
    if let Ok(raw) = std::env::var("HERDR_PLUGIN_EVENT_JSON") {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            // the envelope is { event, data: { .. } }; accept either shape
            let found = v
                .pointer(&format!("/data/{key}"))
                .or_else(|| v.get(key))
                .and_then(Value::as_str);
            if let Some(id) = found {
                return Some(id.to_string());
            }
        }
    }
    let var = match what {
        "tab" => "HERDR_TAB_ID",
        "pane" => "HERDR_PANE_ID",
        _ => "HERDR_WORKSPACE_ID",
    };
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

pub async fn intercept(env: Env, what: &str) -> Result<()> {
    // No config gate. Every guard below is a positive test for an object that
    // is junk by construction — created inside a live mirror workspace, sitting
    // in the `.mirror-pane` placeholder, unmapped, and named by this very
    // event — so there is nothing here a user would want to opt out of that
    // they would not rather have fixed. An unreadable config still stops us,
    // because without hosts we cannot tell a mirror workspace from any other.
    let Ok(config) = load_config(&env.config_search) else { return Ok(()) };
    // Nothing to recreate the object on if the daemon cannot act: with it
    // stopped or paused we would close the local tab, create a real one on the
    // remote, and no mirror would ever come back — the failure being silent
    // precisely because nothing errored.
    if crate::daemon::running_pid(&env).is_none() || crate::daemon::is_paused(&env) {
        return Ok(());
    }
    let Some(target) = event_object_id(what) else { return Ok(()) };
    // let the create→focus pair settle before the first look
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let placeholder = env.state_dir.join(".mirror-pane");
    let api = ApiClient::connect(&env.local_socket).await?;

    match what {
        "tab" | "pane" => {
            intercept_in_mirror(&env, &api, &config, &placeholder, what, &target).await
        }
        _ => intercept_workspace(&env, &api, &placeholder, &target).await,
    }
}

async fn intercept_workspace(
    env: &Env,
    api: &ApiClient,
    placeholder: &std::path::Path,
    target: &str,
) -> Result<()> {
    let ws: Value = api.request("workspace.list", json!({})).await?;
    // the workspace the EVENT named, not whatever happens to be focused now
    let Some(w) = ws.get("workspaces").and_then(Value::as_array).and_then(|a| {
        a.iter().find(|w| w.get("workspace_id").and_then(Value::as_str) == Some(target))
    }) else {
        return Ok(());
    };
    // A fixed directory does not identify the originating workspace. Keep
    // native workspace interception restricted to our private placeholder.
    if w.get("label").and_then(Value::as_str) != Some(".mirror-pane")
        || w.get("pane_count").and_then(Value::as_u64) != Some(1)
    {
        return Ok(());
    }
    let Some(ws_id) = w.get("workspace_id").and_then(Value::as_str) else { return Ok(()) };

    let panes: Value = api.request("pane.list", json!({})).await?;
    let Some(p) = panes.get("panes").and_then(Value::as_array).and_then(|a| {
        a.iter().find(|p| p.get("workspace_id").and_then(Value::as_str) == Some(ws_id))
    }) else {
        return Ok(());
    };
    if !is_bare_placeholder(p, placeholder, None) {
        return Ok(());
    }

    api.request("workspace.close", json!({ "workspace_id": ws_id })).await?;
    open_popup(api, env).await
}

fn is_bare_placeholder(pane: &Value, placeholder: &std::path::Path, fixed_cwd: Option<&std::path::Path>) -> bool {
    let Some(cwd) = pane.get("cwd").and_then(Value::as_str) else { return false };
    let cwd = std::path::Path::new(cwd);
    pane.get("agent").is_none_or(Value::is_null)
        && (crate::cwd_policy::same_path(cwd, placeholder)
            || fixed_cwd.is_some_and(|fixed| crate::cwd_policy::same_path(cwd, fixed)))
}

/// The tab and split arms share their discovery: the focused workspace must
/// be a live mirror, and the junk object is whatever holds a bare placeholder
/// pane that the id map does not know.
/// Tell the user why their tab just vanished.
///
/// Goes to the mirror panes, not the pane being closed: that one is a plain
/// local shell with nothing of ours in it, while a mirror has a streamer that
/// can paint its own status row (the one that says "reconnecting in 10s").
///
/// Every mirror pane in the workspace, not just the anchor, because which one
/// the user is looking at during the gap is not knowable here. Closing the tab
/// hands focus back to whichever tab herdr chooses, and the remote round trip
/// takes a second or two before the replacement shows up. Panes in background
/// tabs paint a status row nobody sees, which costs nothing.
fn notice(env: &Env, targets: &[String], what: &str, host: &str) {
    let msg = format!("intercepted: closing the local {what}, creating it on {host}");
    for pane in targets {
        crate::state::set_pane_hint(&env.state_dir, pane, &msg);
        // Nudge it awake. A streamer's event loop sleeps until its next
        // deadline, and with nothing pending that is never — the notice would
        // sit unread until the remote happened to send a frame.
        crate::util::poke_pane_streamer(&env.state_dir, pane);
    }
}

async fn intercept_in_mirror(
    env: &Env,
    api: &ApiClient,
    config: &crate::config::MirrorConfig,
    placeholder: &std::path::Path,
    what: &str,
    target: &str,
) -> Result<()> {
    let panes_all: Value = api.request("pane.list", json!({})).await?;
    // Locate the object the EVENT named and take its workspace from there. The
    // first version used whichever workspace was focused when the hook woke,
    // which is a different thing entirely once a sibling hook has refocused
    // something or the user has moved on.
    let ws_id = panes_all
        .get("panes")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|p| {
                    let key = if what == "tab" { "tab_id" } else { "pane_id" };
                    p.get(key).and_then(Value::as_str) == Some(target)
                })
                .and_then(|p| p.get("workspace_id").and_then(Value::as_str))
        })
        .map(str::to_string);
    let Some(ws_id) = ws_id else { return Ok(()) };
    let ws_id = ws_id.as_str();

    // is the focused workspace a live mirror, and of which host?
    let mut mapped_tabs: std::collections::HashSet<String> = Default::default();
    let mut mapped_panes: std::collections::HashSet<String> = Default::default();
    let mut host = None;
    for h in &config.hosts {
        let state = crate::state::load_state(&env.state_dir, &h.name);
        let is_ours = state
            .workspaces
            .values()
            .any(|e| e.local_id == ws_id && !e.is_tombstoned());
        if is_ours {
            mapped_tabs = state.tabs.values().map(|e| e.local_id.clone()).collect();
            mapped_panes = state.panes.values().map(|e| e.local_id.clone()).collect();
            // the root tab of a fresh mirror is mapped under the workspace,
            // not the tab table — count it as known
            mapped_tabs.extend(
                state.workspaces.values().filter_map(|e| e.root_tab_local_id.clone()),
            );
            host = Some(h.clone());
            break;
        }
    }
    let Some(host) = host else { return Ok(()) };

    let panes: Value = api.request("pane.list", json!({})).await?;
    let all: Vec<&Value> = panes
        .get("panes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|p| p.get("workspace_id").and_then(Value::as_str) == Some(ws_id)).collect())
        .unwrap_or_default();

    // The junk is the object the event named, if it still qualifies — never
    // "the first unmapped placeholder we can find". Scanning is what let a
    // leftover pane become a target for an unrelated later event.
    let fixed_cwd = crate::cwd_policy::local_fixed_path().await;
    let junk = all.iter().find(|p| {
        let pid = p.get("pane_id").and_then(Value::as_str).unwrap_or("");
        let named = if what == "tab" {
            p.get("tab_id").and_then(Value::as_str) == Some(target)
        } else {
            pid == target
        };
        named && !mapped_panes.contains(pid) && is_bare_placeholder(p, placeholder, fixed_cwd.as_deref())
    });
    let Some(junk) = junk else { return Ok(()) };
    let junk_id = junk.get("pane_id").and_then(Value::as_str).unwrap_or("").to_string();
    let junk_tab = junk.get("tab_id").and_then(Value::as_str).unwrap_or("").to_string();

    // Last gate before anything is closed: give the daemon time to claim it.
    // An unmapped placeholder pane is ambiguous — it is equally the shape of a
    // mirror pane the daemon created moments ago and has not mapped yet, and
    // closing one of those is what reaches the REMOTE via close-through.
    if settles_as_ours(env, api, config, &junk_id).await {
        return Ok(());
    }

    // every mirror pane in this workspace, for the notice
    let mirrors: Vec<String> = all
        .iter()
        .filter_map(|p| p.get("pane_id").and_then(Value::as_str))
        .filter(|pid| mapped_panes.contains(*pid))
        .map(str::to_string)
        .collect();

    // a mirrored sibling to anchor the remote action's context on
    let sibling_in = |tab_only: bool| {
        all.iter()
            .find(|p| {
                let pid = p.get("pane_id").and_then(Value::as_str).unwrap_or("");
                mapped_panes.contains(pid)
                    && (!tab_only || p.get("tab_id").and_then(Value::as_str) == Some(junk_tab.as_str()))
            })
            .and_then(|p| p.get("pane_id").and_then(Value::as_str))
            .map(str::to_string)
    };

    // Runaway breaker: acting more than once per few seconds means the
    // guards misjudged something (say, a daemon map write outran the settle
    // delay) and we are eating our own mirror-back objects. One action per
    // window caps any such feedback loop at nuisance level.
    let marker = env.state_dir.join("intercept-acted");
    let recently = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e < std::time::Duration::from_secs(4));
    if recently {
        return Ok(());
    }
    let mark = || {
        let _ = std::fs::write(&marker, b"");
    };

    match what {
        // native new tab: the junk pane's tab is NOT a mirror tab
        "tab" if !mapped_tabs.contains(&junk_tab) => {
            let Some(anchor) = sibling_in(false) else { return Ok(()) };
            let before = tab_ids_in(api, ws_id).await;
            mark();
            notice(env, &mirrors, "tab", &host.name);
            api.request("tab.close", json!({ "tab_id": junk_tab })).await?;
            run_remote(env, ws_id, &anchor, "tab", None).await?;
            focus_new_tab(api, ws_id, &before).await;
            Ok(())
        }
        // native split: the junk pane sits INSIDE a mirror tab
        "pane" if mapped_tabs.contains(&junk_tab) => {
            // The junk's spot in the LOCAL split tree names both the pane the
            // user actually split and the direction: its parent split's other
            // child is the origin (nearest leaf first), and the parent's
            // direction is the one the user picked. Anchoring there — not on
            // whichever mirrored pane the tab happens to list first — keeps a
            // split of the tab's Nth pane splitting the Nth REMOTE pane, and
            // keeps "split down" splitting down in tabs with several panes.
            let located = junk_origin(api, &junk_tab, &junk_id, &mapped_panes).await;
            let (anchor, dir) = match located {
                Some(x) => x,
                // tree unavailable — fall back to any mirrored pane in the tab
                None => match sibling_in(true) {
                    Some(a) => (a, "right".to_string()),
                    None => return Ok(()),
                },
            };
            mark();
            notice(env, &mirrors, "split", &host.name);
            api.request("pane.close", json!({ "pane_id": junk_id })).await?;
            run_remote(env, ws_id, &anchor, "split", Some(dir.as_str())).await
        }
        _ => Ok(()),
    }
}

/// The mapped pane the junk split off of, plus the split's direction, read
/// from the local layout tree: pane.split hangs the new pane and its origin
/// under a fresh parent split, so `locate_in_layout` on the junk yields that
/// split's direction (already pane.split vocabulary) and the origin-side
/// leaves nearest-first — the first mapped one is the pane the user split.
async fn junk_origin(
    api: &ApiClient,
    tab_id: &str,
    junk_id: &str,
    mapped_panes: &std::collections::HashSet<String>,
) -> Option<(String, String)> {
    let exported = api.request("layout.export", json!({ "tab_id": tab_id })).await.ok()?;
    let root: crate::mirror::LayoutNode =
        serde_json::from_value(exported.pointer("/layout/root")?.clone()).ok()?;
    let (dir, siblings) = crate::mirror::locate_in_layout(&root, junk_id)?;
    let anchor = siblings.into_iter().find(|s| mapped_panes.contains(s))?;
    Some((anchor, dir))
}

/// Hand the remote-create machinery a context pointing at the mirror, exactly
/// as if the matching plugin action had been invoked from that pane.
/// Set when the caller is the intercept hook, so `remote_action::run` refuses
/// its local fallback. That fallback exists so a bound key outside a mirror
/// still does something useful; here it would recreate, inside a mirrored tab,
/// the very local pane we just closed — and its own comment claims that never
/// happens.
pub const NO_LOCAL_FALLBACK_ENV: &str = "HERDR_MIRROR_NO_LOCAL_FALLBACK";

async fn run_remote(
    env: &Env,
    ws_id: &str,
    pane_id: &str,
    kind: &str,
    direction: Option<&str>,
) -> Result<()> {
    std::env::remove_var("HERDR_PLUGIN_CONTEXT_JSON"); // would carry the junk pane
    std::env::set_var("HERDR_ACTIVE_WORKSPACE_ID", ws_id);
    std::env::set_var("HERDR_ACTIVE_PANE_ID", pane_id);
    std::env::remove_var("HERDR_ACTIVE_PANE_CWD");
    std::env::set_var(NO_LOCAL_FALLBACK_ENV, "1");
    let env2 = Env {
        config_search: env.config_search.clone(),
        state_dir: env.state_dir.clone(),
        local_socket: env.local_socket.clone(),
    };
    crate::remote_action::run_cmd(env2, kind, direction).await
}

async fn tab_ids_in(api: &ApiClient, ws_id: &str) -> std::collections::HashSet<String> {
    let Ok(t) = api.request("tab.list", json!({})).await else { return Default::default() };
    t.get("tabs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|t| t.get("workspace_id").and_then(Value::as_str) == Some(ws_id))
                .filter_map(|t| t.get("tab_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The user pressed "new tab" and expects to land in it; the daemon mirrors
/// the remote tab back unfocused, so follow it. Best-effort with a ceiling.
async fn focus_new_tab(
    api: &ApiClient,
    ws_id: &str,
    before: &std::collections::HashSet<String>,
) {
    for _ in 0..16 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let now = tab_ids_in(api, ws_id).await;
        if let Some(fresh) = now.iter().find(|id| !before.contains(*id)) {
            let _ = api.request("tab.focus", json!({ "tab_id": fresh })).await;
            return;
        }
    }
}

/// The invoking pane's cwd, from the shell-binding env var or the
/// plugin-action context JSON — the same two sources remote_action reads.
/// A mirror pane's local cwd is the `.mirror-pane` placeholder; inheriting
/// that would drop the new local workspace into an empty decoy dir, so it's
/// treated as no cwd at all.
fn invoking_cwd() -> Option<String> {
    let cwd = std::env::var("HERDR_ACTIVE_PANE_CWD")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let ctx = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
            let v: Value = serde_json::from_str(&ctx).ok()?;
            v.get("focused_pane_cwd").and_then(Value::as_str).map(str::to_string)
        })?;
    (!cwd.ends_with("/.mirror-pane")).then_some(cwd)
}

/// One menu line: the pickable name plus a dim subtitle (the cwd the local
/// workspace would inherit; a host's ssh target and default marker).
struct Row {
    main: String,
    sub: String,
}

/// `$HOME/...` → `~/...` for display.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path.starts_with(&h) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

/// Popup leg: draw the menu, take one choice, create, exit. Sync on purpose —
/// the menu is a blocking keyboard loop; only the create at the end is async.
pub fn menu(rt: &tokio::runtime::Runtime, env: Env) -> Result<()> {
    // a broken hosts.toml must not brick the picker: local creation needs no
    // hosts, so degrade to a local-only menu and show why
    let (hosts, default_host, config_note) = match load_config(&env.config_search) {
        Ok(c) => {
            let d = c.default_host().map(|h| h.name.clone());
            (c.hosts, d, None)
        }
        Err(e) => (Vec::new(), None, Some(e.to_string())),
    };
    let local_cwd = std::env::var(CWD_ENV).ok().filter(|s| !s.is_empty());
    let mut rows = vec![Row {
        main: "this machine".into(),
        sub: local_cwd.as_deref().map(tilde).unwrap_or_default(),
    }];
    for h in &hosts {
        let mut sub = if h.target != h.name { h.target.clone() } else { String::new() };
        if hosts.len() > 1 && Some(&h.name) == default_host.as_ref() {
            if !sub.is_empty() {
                sub.push(' ');
            }
            sub.push_str("(default)");
        }
        rows.push(Row { main: h.name.clone(), sub });
    }

    let choice = run_menu(&rows, config_note.as_deref())?;
    let outcome = match choice {
        None => {
            println!("cancelled");
            Ok(())
        }
        Some(0) => rt.block_on(create_local(&env)),
        Some(i) => rt.block_on(create_remote(&env, hosts[i - 1].clone())),
    };
    if let Err(e) = &outcome {
        println!("failed: {e}");
    }
    // the popup vanishes when this process exits; hold it long enough to read
    if choice.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(1200));
    }
    outcome
}

async fn create_local(env: &Env) -> Result<()> {
    let api = ApiClient::connect(&env.local_socket).await?;
    // An explicit cwd, always. `invoking_cwd` deliberately refuses to pass the
    // `.mirror-pane` placeholder along, but sending no cwd is not the same as
    // sending a safe one: herdr's default policy is Follow, so it re-derives the
    // cwd from the focused pane — which, when the picker was summoned from a
    // mirror, is that very placeholder. The workspace then looks exactly like
    // the native junk the interception exists to close, so picking "this
    // machine" from inside a mirror destroyed itself.
    let cwd = std::env::var(CWD_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .filter(|s| !s.is_empty());
    let res: Value = api.request("workspace.create", json!({ "cwd": cwd, "focus": true })).await?;
    println!(
        "created local workspace {}",
        res.pointer("/workspace/workspace_id").and_then(Value::as_str).unwrap_or("?")
    );
    Ok(())
}

async fn create_remote(env: &Env, host: HostConfig) -> Result<()> {
    println!("connecting to {}...", host.name);
    let mut remote = RemoteHost::new(&host, &env.state_dir);
    let (api, _status) = remote.connect_api().await?;

    // ids of the mirror workspaces that exist NOW, so the one the daemon is
    // about to build for our create is recognizable as the new arrival
    let local = ApiClient::connect(&env.local_socket).await?;
    let known = local_workspace_ids(&local).await.unwrap_or_default();

    let res: Value = api.request("workspace.create", json!({ "focus": false })).await?;
    println!(
        "created workspace {} on {} - waiting for its mirror...",
        res.pointer("/workspace/workspace_id").and_then(Value::as_str).unwrap_or("?"),
        host.name
    );

    // Follow through: a local create lands you in the new workspace, so a
    // remote one should too. Poll for the mirror the daemon builds (labels are
    // "<host>: <label>") and focus it. Best-effort — a slow mirror just means
    // the message below and the workspace appearing on its own.
    let prefix = format!("{}: ", host.name);
    for _ in 0..16 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let Ok(ws) = local.request("workspace.list", json!({})).await else { continue };
        let Some(arr) = ws.get("workspaces").and_then(Value::as_array) else { continue };
        let fresh = arr.iter().find(|w| {
            let id = w.get("workspace_id").and_then(Value::as_str).unwrap_or("");
            let label = w.get("label").and_then(Value::as_str).unwrap_or("");
            !known.contains(id) && label.starts_with(&prefix)
        });
        if let Some(w) = fresh {
            let id = w.get("workspace_id").and_then(Value::as_str).unwrap_or("");
            let _ = local.request("workspace.focus", json!({ "workspace_id": id })).await;
            println!("opened {}", w.get("label").and_then(Value::as_str).unwrap_or(id));
            return Ok(());
        }
    }
    println!("mirror is taking longer than usual; it will appear on its own");
    Ok(())
}

async fn local_workspace_ids(api: &ApiClient) -> Result<std::collections::HashSet<String>> {
    let ws: Value = api.request("workspace.list", json!({})).await?;
    Ok(ws
        .get("workspaces")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|w| w.get("workspace_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

// --- the menu itself -------------------------------------------------------

/// Returns the selected row index, or None on cancel.
fn run_menu(rows: &[Row], note: Option<&str>) -> Result<Option<usize>> {
    let _raw = RawMode::enable();
    let mut out = std::io::stdout().lock();
    let mut sel = 0usize;
    // Drain whatever is already buffered on stdin before the first draw: the
    // shell prompt that ran just before us may have queried the terminal
    // (cursor position, device attributes), and the response races into our
    // raw-mode read. Nothing buffered at this instant can be the user's
    // answer — the menu hasn't been shown yet.
    while stdin_readable(0) {
        if read_byte().is_err() {
            break;
        }
    }
    // hide the cursor and ask for mouse reports: this is OUR pane, nothing to
    // select in it, so the grab costs nothing and buys click-to-pick. The
    // guard's Drop releases both with the tty.
    let _ = out.write_all(b"\x1b[?25l\x1b[?1000h\x1b[?1006h");

    loop {
        draw(&mut out, rows, sel, note);
        match read_key()? {
            Key::Up => sel = sel.checked_sub(1).unwrap_or(rows.len() - 1),
            Key::Down => sel = (sel + 1) % rows.len(),
            Key::Digit(d) if (d as usize) <= rows.len() && d >= 1 => {
                finish(&mut out);
                return Ok(Some(d as usize - 1));
            }
            // a click on an option row picks it outright; anywhere else ignores
            Key::Click { y } => {
                if let Some(idx) = (y as usize).checked_sub(FIRST_ROW_Y) {
                    if idx < rows.len() {
                        finish(&mut out);
                        return Ok(Some(idx));
                    }
                }
            }
            Key::Enter => {
                finish(&mut out);
                return Ok(Some(sel));
            }
            Key::Cancel => {
                finish(&mut out);
                return Ok(None);
            }
            _ => {}
        }
    }
}

/// 1-based terminal row of the first option line — draw() emits two blank
/// lines, the title, the rule, and one more blank line above the options.
/// Screen row (1-based, as SGR mouse reports them) of the first option. `draw`
/// positions each row absolutely from this, and the click handler subtracts it,
/// so the two can no longer disagree about where the list actually is.
const FIRST_ROW_Y: usize = 6;

fn draw(out: &mut impl Write, rows: &[Row], sel: usize, note: Option<&str>) {
    // full redraw each keypress: the popup is tiny and this keeps it stateless
    let cols = term_cols();
    let name_w = rows.iter().map(|r| r.main.len()).max().unwrap_or(0);
    let rule_w = cols.saturating_sub(8).min(60);
    let _ = out.write_all(b"\x1b[2J\x1b[H");
    let _ = write!(out, "\r\n\r\n    \x1b[1mNew workspace on...\x1b[0m\r\n");
    let _ = write!(out, "    \x1b[2m{}\x1b[0m\r\n\r\n", "─".repeat(rule_w));
    for (i, row) in rows.iter().enumerate() {
        // Absolute, not "wherever the line feeds landed". The popup used to
        // emit one line more than the pty had rows, so every draw scrolled by
        // one while the click math still assumed the unscrolled layout: every
        // mouse pick selected the row above the one clicked.
        let _ = write!(out, "\x1b[{};1H", FIRST_ROW_Y + i);
        // "    ❯ 1 name   sub" — name column aligned so the subs line up; the
        // sub is dimmed and truncated to the popup width
        let sub_room = cols.saturating_sub(name_w + 14);
        let sub: String = row.sub.chars().take(sub_room).collect();
        if i == sel {
            let _ = write!(
                out,
                "    \x1b[7;1m > {} {:name_w$} \x1b[0m\x1b[2m  {}\x1b[0m\r\n",
                i + 1,
                row.main,
                sub
            );
        } else {
            let _ = write!(out, "      {} {:name_w$}\x1b[2m   {}\x1b[0m\r\n", i + 1, row.main, sub);
        }
    }
    let _ = write!(out, "\r\n    \x1b[2m{}\x1b[0m\r\n", "─".repeat(rule_w));
    let _ = write!(out, "    \x1b[2mclick or enter pick - up/down move - esc cancel\x1b[0m\r\n");
    if let Some(n) = note {
        let _ = write!(out, "    \x1b[2m(hosts.toml: {n})\x1b[0m\r\n");
    }
    let _ = out.flush();
}

fn term_cols() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

fn finish(out: &mut impl Write) {
    let _ = out.write_all(b"\x1b[2J\x1b[H\x1b[?25h\r\n  ");
    let _ = out.flush();
    // swallow whatever trails the deciding event (a click's release sequence,
    // a queued repeat) so it can't leak into the pane as typed garbage
    while stdin_readable(0) {
        if read_byte().is_err() {
            break;
        }
    }
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Digit(u8),
    Click { y: u32 },
    Other,
}

/// One byte straight off fd 0. NOT std::io::stdin(): that handle buffers, so
/// its first read swallows an arrow key's `[B` tail into userspace where the
/// poll() below can't see it — turning every arrow into a bare-ESC cancel.
fn read_byte() -> Result<u8> {
    let mut b = 0u8;
    loop {
        let n = unsafe { libc::read(libc::STDIN_FILENO, &mut b as *mut u8 as *mut libc::c_void, 1) };
        match n {
            1 => return Ok(b),
            0 => return Err(crate::util::err("stdin closed")),
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return Err(std::io::Error::last_os_error().into()),
        }
    }
}

/// One keypress from raw stdin. A lone ESC cancels; ESC [ A/B are arrows —
/// told apart by polling for the follow-up byte instead of blocking on it.
///
/// Escape sequences are consumed WHOLE. The terminal answers queries (cursor
/// position, device attributes) on this same stdin, and a parser that takes a
/// fixed two bytes after ESC leaves the tail of `ESC [ 12 ; 34 R` behind —
/// where the digits then read as direct menu selections. That is not
/// hypothetical: the shell prompt's own query response raced the first draw
/// and "picked" a host on its own.
fn read_key() -> Result<Key> {
    Ok(match read_byte()? {
        b'\r' | b'\n' => Key::Enter,
        b'q' | 0x03 => Key::Cancel, // q, ctrl+c
        b'k' => Key::Up,
        b'j' => Key::Down,
        d @ b'1'..=b'9' => Key::Digit(d - b'0'),
        0x1b => {
            if !stdin_readable(300) {
                return Ok(Key::Cancel); // bare ESC
            }
            match read_byte()? {
                // CSI: parameter/intermediate bytes end at a final 0x40-0x7E
                b'[' => {
                    let mut fin = 0u8;
                    let mut params = Vec::new();
                    while stdin_readable(60) {
                        let b = read_byte()?;
                        if (0x40..=0x7e).contains(&b) {
                            fin = b;
                            break;
                        }
                        params.push(b);
                    }
                    match fin {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        // SGR mouse `<btn;x;y` + M(press)/m(release): wheel
                        // moves the selection, a left-press picks by row
                        b'M' | b'm' if params.first() == Some(&b'<') => {
                            let f: Vec<u32> = String::from_utf8_lossy(&params[1..])
                                .split(';')
                                .filter_map(|s| s.parse().ok())
                                .collect();
                            match (f.first(), f.get(2), fin) {
                                (Some(64), _, b'M') => Key::Up,
                                (Some(65), _, b'M') => Key::Down,
                                (Some(0), Some(&y), b'M') => Key::Click { y },
                                _ => Key::Other,
                            }
                        }
                        _ => Key::Other,
                    }
                }
                // SS3 (application cursor keys): one final byte
                b'O' => match if stdin_readable(60) { read_byte()? } else { 0 } {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    _ => Key::Other,
                },
                _ => Key::Other,
            }
        }
        _ => Key::Other,
    })
}

/// poll(2) stdin: is a byte already waiting?
fn stdin_readable(timeout_ms: i32) -> bool {
    let mut fds = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut fds, 1, timeout_ms) > 0 }
}

/// Same raw-mode guard as pane.rs, plus Drop: the menu has many exits and
/// every one of them must give the shell its terminal back.
struct RawMode {
    orig: Option<libc::termios>,
}

impl RawMode {
    fn enable() -> RawMode {
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                return RawMode { orig: None };
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return RawMode { orig: None };
            }
            RawMode { orig: Some(orig) }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(orig) = &self.orig {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
            }
        }
        // cursor back on and mouse released even if finish() was never reached
        let _ = std::io::stdout().write_all(b"\x1b[?1000l\x1b[?25h");
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::*;

    #[test]
    fn fixed_path_requires_agentless_pane_and_explicit_opt_in() {
        let placeholder = std::path::Path::new("/private/mirror-placeholder");
        let fixed = std::path::Path::new("/projects");
        let pane = json!({"cwd": "/projects", "agent": null});
        assert!(is_bare_placeholder(&pane, placeholder, Some(fixed)));
        assert!(!is_bare_placeholder(&pane, placeholder, None));
        assert!(!is_bare_placeholder(&json!({"cwd": "/projects", "agent": "claude"}), placeholder, Some(fixed)));
        assert!(!is_bare_placeholder(&json!({"agent": null}), placeholder, Some(fixed)));
    }

    #[test]
    fn fixed_path_matches_a_symlink_alias() {
        let root = std::env::temp_dir().join(format!("mirror-cwd-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let pane = json!({"cwd": root.join("real").canonicalize().unwrap(), "agent": null});
        let matched = is_bare_placeholder(&pane, &root.join("placeholder"), Some(&root.join("alias")));
        let different = is_bare_placeholder(&pane, &root.join("placeholder"), Some(&root.join("missing")));
        std::fs::remove_dir_all(root).unwrap();
        assert!(matched);
        assert!(!different);
    }

    #[tokio::test]
    #[ignore = "requires PR89_LOCAL_SOCKET pointing to an isolated test Herdr"]
    async fn fixed_local_workspace_is_not_intercepted() {
        let root = std::path::PathBuf::from(std::env::var("PR89_LIVE_ROOT").unwrap());
        let env = Env {config_search: vec![], state_dir: root.clone(),
            local_socket: std::env::var("PR89_LOCAL_SOCKET").unwrap().into()};
        let api = ApiClient::connect(&env.local_socket).await.unwrap();
        let ws = api.request("workspace.create", json!({"focus": false})).await.unwrap();
        let id = ws["workspace"]["workspace_id"].as_str().unwrap();
        let fixed = crate::cwd_policy::local_fixed_path().await.expect("fixed test policy");
        assert!(is_bare_placeholder(&ws["root_pane"], &root.join(".mirror-pane"), Some(&fixed)));
        intercept_workspace(&env, &api, &root.join(".mirror-pane"), id).await.unwrap();
        let all = api.request("workspace.list", json!({})).await.unwrap();
        assert!(all["workspaces"].as_array().unwrap().iter().any(|w| w["workspace_id"] == id));
        api.request("workspace.close", json!({"workspace_id": id})).await.unwrap();
    }
}
