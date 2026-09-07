use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use super::*;
use crate::state::{save_state, HostState, PaneEntry};

fn pane_log(id: &str, pane: &str) -> Value {
    json!({
        "log_id": id, "plugin_id": "review", "status": "succeeded", "exit_code": 0,
        "stdout": json!({"result":{"plugin_pane":{
            "plugin_id":"review", "pane":{"pane_id":pane,"focused":true}
        }}}).to_string()
    })
}

async fn exercise(navigate_away: bool) -> Vec<String> {
    let root = std::env::temp_dir().join(format!("mirror-action-focus-{}-{navigate_away}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let env = Env { config_search: vec![], state_dir: root.clone(), local_socket: socket.clone() };
    let focused = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_focused = focused.clone();
    let server_root = root.clone();
    let server = tokio::spawn(async move {
        let mut log_polls = 0;
        let mut snapshots = 0;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            let line = BufReader::new(read).lines().next_line().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "ping" => json!({}),
                "plugin.log.list" => {
                    assert_eq!(request["params"]["plugin_id"], "review");
                    log_polls += 1;
                    let wanted = if log_polls == 1 {
                        json!({"log_id":"wanted", "status":"running"})
                    } else { pane_log("wanted", "remote-review") };
                    // A different action finishes first and is newest in the log.
                    json!({"logs":[wanted, pane_log("other", "unrelated-review")]})
                }
                "session.snapshot" => {
                    snapshots += 1;
                    if snapshots == 2 {
                        let mut state = HostState::default();
                        state.panes.insert("remote-review".into(), PaneEntry {
                            local_id:"local-review".into(), tombstone:None, seq:0, reported:None,
                        });
                        save_state(&server_root, "host", &state).unwrap();
                    }
                    json!({"snapshot":{
                        "focused_pane_id":if navigate_away {"user-chosen"} else {"source"},
                        "panes":[{"pane_id":"source"},{"pane_id":"local-review"}]
                    }})
                }
                "pane.focus" => {
                    server_focused.lock().unwrap().push(request["params"]["pane_id"].as_str().unwrap().into());
                    json!({})
                }
                other => panic!("unexpected API request {other}"),
            };
            write.write_all(format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes()).await.unwrap();
        }
    });
    let api = ApiClient::connect(&socket).await.unwrap();
    follow(&env, &InvocationContext {workspace_id:Some("local-workspace".into()),focused_pane_id:Some("source".into())},
        "host", &api, &json!({"log":{"log_id":"wanted","plugin_id":"review"}})).await.unwrap();
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(root).unwrap();
    Arc::try_unwrap(focused).unwrap().into_inner().unwrap()
}

#[tokio::test]
async fn follows_its_own_completed_action_after_the_mirror_mapping_arrives() {
    assert_eq!(exercise(false).await, ["local-review"]);
}

#[tokio::test]
async fn user_navigation_during_the_action_is_not_overridden() {
    assert!(exercise(true).await.is_empty());
}

#[test]
fn only_an_explicit_focused_pane_from_the_invoked_plugin_is_followed() {
    let log = pane_log("wanted", "remote-review");
    assert_eq!(focused_result(&log, "review").as_deref(), Some("remote-review"));
    assert_eq!(focused_result(&log, "other-plugin"), None);
    for stdout in [
        "plain action output".to_owned(),
        json!({"result":{"pane":{"pane_id":"not-a-plugin-pane","focused":true}}}).to_string(),
        json!({"result":{"plugin_pane":{"plugin_id":"review","pane":{"pane_id":"background","focused":false}}}}).to_string(),
    ] {
        let mut background = log.clone();
        background["stdout"] = json!(stdout);
        assert_eq!(focused_result(&background, "review"), None);
    }
}

#[test]
fn context_comes_from_the_invoking_remote_pane_even_when_global_focus_differs() {
    let snapshot = json!({
        "focused_workspace_id":"other", "focused_tab_id":"other-tab", "focused_pane_id":"other-pane",
        "workspaces":[{"workspace_id":"remote-ws","cwd":"/project","label":"Project"}],
        "tabs":[{"tab_id":"remote-tab","label":"Code"}],
        "panes":[{"pane_id":"remote-pane","workspace_id":"remote-ws","tab_id":"remote-tab",
            "cwd":"/project","foreground_cwd":"/project/docs","agent":"codex","agent_status":"done"}]
    });
    let context = super::super::remote_context(&snapshot, "remote-pane").unwrap();
    assert_eq!(context["workspace_id"], "remote-ws");
    assert_eq!(context["tab_id"], "remote-tab");
    assert_eq!(context["workspace_cwd"], "/project");
    assert_eq!(context["focused_pane_cwd"], "/project/docs");
    assert_eq!(context["focused_pane_agent"], "codex");
    assert!(super::super::remote_context(&snapshot, "closed-pane").is_err());
    let mut shell = snapshot;
    shell["panes"][0].as_object_mut().unwrap().remove("agent");
    assert_eq!(super::super::remote_context(&shell, "remote-pane").unwrap()["focused_pane_agent"], "");
}
