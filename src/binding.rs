// Keybinding setup CLI: discovery plus explicit, echoed edits to herdr's
// config. Nothing here runs in the background — every mutation is a command
// the user typed, printed back as it happens.
//
//   herdr-mirror remote-actions [host]      # list invokable plugin actions
//   herdr-mirror bind <plugin>.<action> <key>
//   herdr-mirror unbind <plugin>.<action> | <key>
//
// `bind` appends one marked [[keys.command]] block to herdr's config.toml and
// reloads the server, so the key works when the command returns. `unbind`
// removes only blocks carrying our marker — hand-written bindings are never
// touched, even for the same action.

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::load_config;
use crate::remote::RemoteHost;
use crate::util::{err, herdr_config_path, Env, Result};

/// The stable CLI path install.sh links; used verbatim in written bindings so
/// they don't depend on the login-sh PATH (see the README's Remote plugin
/// keys section).
const CLI_PATH: &str = "~/.local/bin/herdr-mirror";

const MARKER: &str = "# herdr-mirror bind:";

/// herdr constrains plugin/action ids to this charset at manifest load; a
/// spec is such ids joined by dots. Enforcing it here keeps a hostile or
/// mistyped spec out of the TOML string and out of the `sh -lc` line herdr
/// executes on every keypress — `remote-actions` output is remote-supplied,
/// and copy-pasting it into `bind` is the documented workflow.
fn valid_spec(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
}

/// Key combos are alphanumeric segments joined by '+' (prefix+alt+l, f1,
/// minus). Anything outside that can't be safely quoted into the TOML block,
/// so it's refused rather than escaped.
fn valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-'))
}

/// Replace control characters before terminal output — remote-supplied titles
/// must not be able to smuggle escape sequences into the user's terminal.
fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Is `key` already the value of some binding line? Covers `key = "<k>"` in
/// [[keys.command]] AND named overrides like `new_tab = "<k>"`, both quote
/// styles, ignoring comments. herdr's built-in defaults aren't in the file so
/// they can't be detected — which is also the documented way to shadow a
/// native key on purpose. (List-valued bindings are not scanned.)
fn key_bound_in(content: &str, key: &str) -> bool {
    let (dq, sq) = (format!("\"{key}\""), format!("'{key}'"));
    content.lines().any(|l| {
        let l = l.trim();
        if l.starts_with('#') {
            return false;
        }
        let Some((_, v)) = l.split_once('=') else { return false };
        let v = v.split('#').next().unwrap_or("").trim();
        v == dq || v == sq
    })
}

/// Atomic replace: a crash mid-write must not truncate herdr's config.
fn write_config(path: &PathBuf, content: &str) -> Result<()> {
    let tmp = path.with_extension("toml.herdr-mirror-tmp");
    fs::write(&tmp, content).map_err(|e| err(format!("cannot write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(|e| err(format!("cannot replace {}: {e}", path.display())))
}

fn binding_block(spec: &str, key: &str) -> String {
    format!(
        "\n{MARKER} {spec}\n[[keys.command]]\nkey = \"{key}\"\ntype = \"shell\"\ncommand = \"{CLI_PATH} remote-invoke {spec}\"\n"
    )
}

/// plugin.action.list against one socket, flattened to (spec, title) rows.
async fn list_actions(api: &crate::api::ApiClient) -> Result<Vec<(String, String)>> {
    let res = api.request("plugin.action.list", json!({})).await?;
    let actions = res
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| err("plugin.action.list: no actions array in response"))?;
    Ok(actions
        .iter()
        .filter_map(|a| {
            let plugin = a.get("plugin_id").and_then(Value::as_str)?;
            let action = a.get("action_id").and_then(Value::as_str)?;
            let title = a.get("title").and_then(Value::as_str).unwrap_or("");
            Some((format!("{plugin}.{action}"), title.to_string()))
        })
        .collect())
}

fn print_actions(rows: &[(String, String)]) {
    // a spec outside herdr's id charset is either corruption or an attempt to
    // get shell metacharacters copy-pasted into a binding — never print it
    let (ok, bad): (Vec<_>, Vec<_>) = rows.iter().partition(|(s, _)| valid_spec(s));
    let width = ok.iter().map(|(s, _)| s.len()).max().unwrap_or(0);
    for (spec, title) in &ok {
        println!("  {spec:width$}  {}", printable(title));
    }
    if !bad.is_empty() {
        println!("  (skipped {} action(s) with non-standard ids)", bad.len());
    }
}

/// `remote-actions [host]`: what `remote-invoke` can reach. No argument lists
/// every configured host, then the local herdr last — the remote side is the
/// part you can't see from here, so it leads. A host name (or "local")
/// narrows to that one. Unreachable hosts report and don't abort the rest.
pub async fn remote_actions(env: Env, host_arg: Option<&str>) -> Result<()> {
    if host_arg != Some("local") {
        // remote-actions is a discovery command, so unlike remote-invoke a
        // missing/broken hosts.toml only matters when hosts were asked for
        let hosts = match load_config(&env.config_search) {
            Ok(c) => c.hosts,
            Err(e) if host_arg.is_none() => {
                println!("(no hosts listed: {e})");
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        if let Some(name) = host_arg {
            if !hosts.iter().any(|h| h.name == name) {
                let known: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
                return Err(err(format!(
                    "unknown host {name:?} (configured: {})",
                    if known.is_empty() { "none".into() } else { known.join(", ") }
                )));
            }
        }
        for host in &hosts {
            if host_arg.is_some_and(|name| name != host.name) {
                continue;
            }
            // an ssh connect can take seconds; say so instead of hanging mute
            println!("connecting to {}...", host.name);
            let mut remote = RemoteHost::new(host, &env.state_dir);
            match remote.connect_api().await {
                Ok((api, _)) => match list_actions(&api).await {
                    Ok(rows) => {
                        println!("{}:", host.name);
                        print_actions(&rows);
                    }
                    Err(e) => println!("{}: (cannot list: {e})", host.name),
                },
                Err(e) => println!("{}: (unreachable: {e})", host.name),
            }
            println!();
        }
    }

    if matches!(host_arg, None | Some("local")) {
        println!("local:");
        let api = crate::api::ApiClient::connect(&env.local_socket).await?;
        print_actions(&list_actions(&api).await?);
    }

    println!();
    println!("bind one (writes the keybinding and reloads herdr):");
    println!("  herdr-mirror bind <plugin>.<action> <key>");
    println!("or paste into {}:", herdr_config_path().display());
    print!("{}", binding_block("<plugin>.<action>", "prefix+alt+..."));
    Ok(())
}

/// `bind <plugin>.<action> <key>`: append the marked binding block to herdr's
/// config and reload the server. Refuses a key that's already bound anywhere
/// in the file (a duplicate key would be ambiguous, and this tool only ever
/// appends); a spec bound by us already is a no-op.
pub async fn bind(env: Env, spec: &str, key: &str) -> Result<()> {
    if !valid_spec(spec) || matches!(spec.split_once('.'), Some((p, a)) if p.is_empty() || a.is_empty())
    {
        return Err(err(format!(
            "bad action spec {spec:?}: want <plugin>.<action> using only letters, digits, and :._-"
        )));
    }
    if !valid_key(key) {
        return Err(err(format!(
            "bad key {key:?}: want segments joined by '+' using only letters, digits, and _-"
        )));
    }

    // Connect (and probe the action) BEFORE touching the file: a down server
    // aborts with nothing changed, and the read-check-write window that races
    // herdr's own config writes stays as small as possible.
    let api = crate::api::ApiClient::connect(&env.local_socket).await?;
    match list_actions(&api).await {
        Ok(rows) if !rows.iter().any(|(s, _)| s == spec) => {
            println!("note: {spec} is not on the local herdr; assuming it exists on the remote");
        }
        _ => {}
    }
    if !crate::util::cli_link_path().exists() {
        println!(
            "warning: {} does not exist yet — the binding won't fire until it does (Mirror: start repairs it)",
            crate::util::cli_link_path().display()
        );
    }

    let path = herdr_config_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("note: {} does not exist; creating it", path.display());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            String::new()
        }
        Err(e) => return Err(err(format!("cannot read {}: {e}", path.display()))),
    };

    if content.contains(&format!("{MARKER} {spec}\n")) {
        println!("{spec} is already bound by herdr-mirror in {}", path.display());
        return Ok(());
    }
    if key_bound_in(&content, key) {
        return Err(err(format!(
            "{key} is already bound in {}; pick another key or edit the file",
            path.display()
        )));
    }

    let block = binding_block(spec, key);
    write_config(&path, &format!("{content}{block}"))?;
    println!("appended to {}:{block}", path.display());

    api.request("server.reload_config", json!({})).await?;
    println!("reloaded herdr config; {key} is live");
    Ok(())
}

/// `unbind <plugin>.<action> | <key>`: remove the marked block matching the
/// spec (marker line) or the key (its `key = "..."` line), plus the blank
/// line the append added. Only marker-carrying blocks are candidates.
pub async fn unbind(env: Env, what: &str) -> Result<()> {
    // connect first, like bind: a down server must not leave the file edited
    // but the running config stale
    let api = crate::api::ApiClient::connect(&env.local_socket).await?;

    let path = herdr_config_path();
    let content = fs::read_to_string(&path)
        .map_err(|e| err(format!("cannot read {}: {e}", path.display())))?;

    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut removed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(spec) = line.trim().strip_prefix(MARKER) {
            // our blocks are exactly marker + 4 lines (see binding_block);
            // verify the shape before touching anything so a hand-edited
            // block or a marker-lookalike comment never loses a neighbor line
            let block = &lines[i..(i + 5).min(lines.len())];
            let is_our_block = block.len() == 5
                && block[1].trim() == "[[keys.command]]"
                && block[2].trim().starts_with("key = \"")
                && block[3].trim() == "type = \"shell\""
                && block[4].trim().starts_with(&format!("command = \"{CLI_PATH} remote-invoke "));
            let key_hit = block.iter().any(|l| l.trim() == format!("key = \"{what}\""));
            if is_our_block && (spec.trim() == what || key_hit) {
                if out.ends_with("\n\n") {
                    out.pop(); // the blank separator the append added
                }
                print!("removing from {}:\n{}", path.display(), block.concat());
                i += block.len();
                removed += 1;
                continue;
            }
        }
        out.push_str(line);
        i += 1;
    }

    if removed == 0 {
        return Err(err(format!(
            "no herdr-mirror-managed binding for {what:?} in {} (only intact blocks written by `herdr-mirror bind` are removable)",
            path.display()
        )));
    }
    write_config(&path, &out)?;

    api.request("server.reload_config", json!({})).await?;
    println!("reloaded herdr config");
    Ok(())
}
