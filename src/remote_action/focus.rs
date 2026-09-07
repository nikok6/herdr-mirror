//! Follow a forwarded plugin's explicit pane result, never the remote's global focus.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::{sleep, timeout};

use super::InvocationContext;
use crate::api::ApiClient;
use crate::state::load_state;
use crate::util::{err, Env, Result};

/// `plugin.action.invoke` returns a running command log, not the pane it will
/// open. Watch that exact log; another invocation may finish first. Plugins
/// which don't print a focused `plugin_pane` result retain their old behavior.
pub(super) async fn follow(
    env: &Env,
    context: &InvocationContext,
    host: &str,
    remote: &ApiClient,
    invocation: &Value,
) -> Result<()> {
    let Some(log_id) = invocation.pointer("/log/log_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(plugin_id) = invocation.pointer("/log/plugin_id").and_then(Value::as_str) else {
        return Ok(());
    };
    // Some actions are themselves long-running; keep them asynchronous after
    // this bounded opportunity to follow a pane, rather than killing the action.
    let completed = timeout(Duration::from_secs(15), completed_log(remote, plugin_id, log_id)).await;
    let Ok(completed) = completed else { return Ok(()) };
    let log = completed?;
    if log["status"] == "failed" {
        return Err(err(format!("remote plugin {plugin_id} failed on {host}; inspect {log_id}")));
    }
    let Some(pane_id) = focused_result(&log, plugin_id) else { return Ok(()) };
    let local = ApiClient::connect(&env.local_socket).await?;
    timeout(Duration::from_secs(15), focus_when_mapped(env, context, host, &local, &pane_id))
        .await
        .map_err(|_| err(format!("plugin opened {pane_id} on {host}, but its mirror did not appear; check the mirror daemon")))?
}

async fn completed_log(remote: &ApiClient, plugin_id: &str, log_id: &str) -> Result<Value> {
    loop {
        let response = remote.request("plugin.log.list", json!({"plugin_id": plugin_id, "limit": 200})).await?;
        if let Some(log) = response["logs"].as_array().into_iter().flatten().find(|l| l["log_id"] == log_id) {
            if log["status"] != "running" {
                return Ok(log.clone());
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn focused_result(log: &Value, plugin_id: &str) -> Option<String> {
    if log["status"] != "succeeded" {
        return None;
    }
    let result: Value = serde_json::from_str(log["stdout"].as_str()?).ok()?;
    let plugin_pane = result.pointer("/result/plugin_pane")?;
    if plugin_pane["plugin_id"] != plugin_id || plugin_pane["pane"]["focused"] != true {
        return None;
    }
    plugin_pane["pane"]["pane_id"].as_str().filter(|p| !p.is_empty()).map(str::to_owned)
}

async fn focus_when_mapped(
    env: &Env,
    context: &InvocationContext,
    host: &str,
    local: &ApiClient,
    remote_pane: &str,
) -> Result<()> {
    let Some(source_pane) = &context.focused_pane_id else { return Ok(()) };
    loop {
        let snapshot = local.request("session.snapshot", json!({})).await?;
        if snapshot["snapshot"]["focused_pane_id"] != *source_pane {
            // Do not pull the user back after they navigate away during SSH or
            // plugin startup. The new mirror remains available in its tab.
            return Ok(());
        }
        let state = load_state(&env.state_dir, host);
        if let Some(entry) = state.panes.get(remote_pane) {
            if entry.is_tombstoned() {
                return Ok(());
            }
            let exists = snapshot["snapshot"]["panes"].as_array().into_iter().flatten()
                .any(|p| p["pane_id"] == entry.local_id);
            if exists {
                local.request("pane.focus", json!({"pane_id": entry.local_id})).await?;
                return Ok(());
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests;
