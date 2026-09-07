// Stream scheduling for mirrored panes.
//
// This module intentionally has no filesystem, API, or clock reads.  The
// daemon owns those effects; keeping the decision here pure makes focus and
// grace-period behaviour straightforward to test.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// The stream policy loaded from `[stream]` in hosts.toml.
#[derive(Debug, Clone)]
pub struct StreamPolicy {
    pub visible_only: bool,
    pub pause_after: Duration,
}

/// The daemon's current knowledge of one mapped local pane.
#[derive(Debug, Clone)]
pub struct PaneStreamState {
    pub local_id: String,
    /// Whether its pause marker is absent, i.e. the streamer should have an
    /// active remote session.
    pub streaming: bool,
    /// When it most recently became invisible. Cleared as soon as it returns
    /// to the focused workspace/tab.
    pub hidden_since: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamAction {
    Start(String),
    Stop(String),
}

/// Return the local panes which are visible in the currently focused view.
///
/// A missing focused workspace is an older-server shape, not evidence that no
/// panes are visible. Keep every pane live in that case so an upgrade cannot
/// silently pause an entire mirror fleet.
pub fn visible_local_panes(snap: &crate::mirror::Snapshot) -> HashSet<String> {
    let Some(focused_workspace_id) = snap.focused_workspace_id.as_deref() else {
        return snap.panes.iter().map(|pane| pane.pane_id.clone()).collect();
    };

    let focused_tab_id = snap
        .focused_tab_id
        .as_deref()
        .or_else(|| {
            snap.workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == focused_workspace_id)
                .and_then(|workspace| workspace.active_tab_id.as_deref())
        });
    let Some(focused_tab_id) = focused_tab_id else {
        return HashSet::new();
    };

    let panes_in_focused_view: HashSet<String> = snap
        .panes
        .iter()
        .filter(|pane| {
            pane.workspace_id == focused_workspace_id && pane.tab_id == focused_tab_id
        })
        .map(|pane| pane.pane_id.clone())
        .collect();

    if let Some(layout) = snap
        .layouts
        .iter()
        .find(|layout| layout.tab_id == focused_tab_id && layout.zoomed)
    {
        return layout
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| panes_in_focused_view.contains(*pane_id))
            .into_iter()
            .cloned()
            .collect();
    }

    panes_in_focused_view
}

/// Decide which pause-marker transitions are required now.
pub fn plan_streams(
    policy: &StreamPolicy,
    link_up: bool,
    visible: &HashSet<String>,
    tracked: &[PaneStreamState],
    now: Instant,
) -> Vec<StreamAction> {
    if !link_up {
        return tracked
            .iter()
            .filter(|pane| pane.streaming)
            .map(|pane| StreamAction::Stop(pane.local_id.clone()))
            .collect();
    }

    if !policy.visible_only {
        return tracked
            .iter()
            .filter(|pane| !pane.streaming)
            .map(|pane| StreamAction::Start(pane.local_id.clone()))
            .collect();
    }

    tracked
        .iter()
        .filter_map(|pane| {
            if visible.contains(&pane.local_id) && !pane.streaming {
                Some(StreamAction::Start(pane.local_id.clone()))
            } else if !visible.contains(&pane.local_id)
                && pane.streaming
                && pane
                    .hidden_since
                    .is_some_and(|hidden_since| now.saturating_duration_since(hidden_since) >= policy.pause_after)
            {
                Some(StreamAction::Stop(pane.local_id.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// The earliest pending visible-only grace expiry, if there is one.
pub fn next_deadline(policy: &StreamPolicy, tracked: &[PaneStreamState]) -> Option<Instant> {
    if !policy.visible_only {
        return None;
    }
    tracked
        .iter()
        .filter(|pane| pane.streaming)
        .filter_map(|pane| pane.hidden_since.and_then(|hidden_since| hidden_since.checked_add(policy.pause_after)))
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> StreamPolicy {
        StreamPolicy { visible_only: true, pause_after: Duration::from_secs(30) }
    }

    fn snapshot_value() -> serde_json::Value {
        json!({
            "focused_workspace_id": "ws-alpha",
            "focused_tab_id": "tab-alpha-1",
            "protocol": "synthetic",
            "version": "test",
            "workspaces": [
                {"workspace_id": "ws-alpha", "label": "alpha", "number": 1, "focused": true,
                 "active_tab_id": "tab-alpha-1", "tab_count": 2, "pane_count": 3, "agent_status": "idle"},
                {"workspace_id": "ws-beta", "label": "beta", "number": 2, "focused": false,
                 "active_tab_id": "tab-beta-1", "tab_count": 2, "pane_count": 2, "agent_status": "idle"}
            ],
            "tabs": [
                {"tab_id": "tab-alpha-1", "workspace_id": "ws-alpha", "label": "alpha-one", "number": 1, "focused": true, "pane_count": 2, "agent_status": "idle"},
                {"tab_id": "tab-alpha-2", "workspace_id": "ws-alpha", "label": "alpha-two", "number": 2, "focused": false, "pane_count": 1, "agent_status": "idle"},
                {"tab_id": "tab-beta-1", "workspace_id": "ws-beta", "label": "beta-one", "number": 1, "focused": false, "pane_count": 1, "agent_status": "idle"},
                {"tab_id": "tab-beta-2", "workspace_id": "ws-beta", "label": "beta-two", "number": 2, "focused": false, "pane_count": 1, "agent_status": "idle"}
            ],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "tab-alpha-1", "workspace_id": "ws-alpha", "focused": true, "cwd": "/tmp", "foreground_cwd": "/tmp", "terminal_title": "one"},
                {"pane_id": "w1:p2", "tab_id": "tab-alpha-1", "workspace_id": "ws-alpha", "focused": false, "cwd": "/tmp", "foreground_cwd": "/tmp", "terminal_title": "two"},
                {"pane_id": "w1:p3", "tab_id": "tab-alpha-2", "workspace_id": "ws-alpha", "focused": false, "cwd": "/tmp", "foreground_cwd": "/tmp", "terminal_title": "three"},
                {"pane_id": "w2:p1", "tab_id": "tab-beta-1", "workspace_id": "ws-beta", "focused": false, "cwd": "/tmp", "foreground_cwd": "/tmp", "terminal_title": "four"},
                {"pane_id": "w2:p2", "tab_id": "tab-beta-2", "workspace_id": "ws-beta", "focused": false, "cwd": "/tmp", "foreground_cwd": "/tmp", "terminal_title": "five"}
            ],
            "layouts": [
                {"tab_id": "tab-alpha-1", "workspace_id": "ws-alpha", "zoomed": false, "focused_pane_id": "w1:p1",
                 "area": {}, "panes": [
                    {"pane_id": "w1:p1", "focused": true, "rect": {"x": 0, "y": 0, "width": 40, "height": 20}},
                    {"pane_id": "w1:p2", "focused": false, "rect": {"x": 40, "y": 0, "width": 40, "height": 20}}
                 ], "splits": []},
                {"tab_id": "tab-alpha-2", "workspace_id": "ws-alpha", "zoomed": false, "focused_pane_id": "w1:p3",
                 "area": {}, "panes": [{"pane_id": "w1:p3", "focused": false, "rect": {"x": 0, "y": 0, "width": 80, "height": 20}}], "splits": []},
                {"tab_id": "tab-beta-1", "workspace_id": "ws-beta", "zoomed": false, "focused_pane_id": "w2:p1",
                 "area": {}, "panes": [{"pane_id": "w2:p1", "focused": false, "rect": {"x": 0, "y": 0, "width": 80, "height": 20}}], "splits": []},
                {"tab_id": "tab-beta-2", "workspace_id": "ws-beta", "zoomed": false, "focused_pane_id": "w2:p2",
                 "area": {}, "panes": [{"pane_id": "w2:p2", "focused": false, "rect": {"x": 0, "y": 0, "width": 80, "height": 20}}], "splits": []}
            ]
        })
    }

    fn snapshot(value: serde_json::Value) -> crate::mirror::Snapshot {
        serde_json::from_value(value).unwrap()
    }

    fn ids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn hidden_pane_survives_the_grace_window() {
        let now = Instant::now();
        let tracked = [PaneStreamState {
            local_id: "local-hidden".into(),
            streaming: true,
            hidden_since: Some(now - Duration::from_secs(5)),
        }];
        assert_eq!(plan_streams(&policy(), true, &HashSet::new(), &tracked, now), Vec::<StreamAction>::new());
    }

    #[test]
    fn hidden_pane_stops_after_the_grace_window() {
        let now = Instant::now();
        let tracked = [PaneStreamState {
            local_id: "local-hidden".into(),
            streaming: true,
            hidden_since: Some(now - Duration::from_secs(31)),
        }];
        assert_eq!(
            plan_streams(&policy(), true, &HashSet::new(), &tracked, now),
            vec![StreamAction::Stop("local-hidden".into())]
        );
    }

    #[test]
    fn only_the_focused_workspace_and_active_tab_are_visible() {
        let visible = visible_local_panes(&snapshot(snapshot_value()));
        assert_eq!(visible, ids(&["w1:p1", "w1:p2"]));
        assert!(!visible.contains("w2:p1"));
        assert!(!visible.contains("w2:p2"));
    }

    #[test]
    fn a_zoomed_tab_shows_only_the_zoomed_pane() {
        let mut value = snapshot_value();
        value["layouts"][0]["zoomed"] = json!(true);
        value["layouts"][0]["focused_pane_id"] = json!("w1:p2");
        assert_eq!(visible_local_panes(&snapshot(value)), ids(&["w1:p2"]));
    }

    #[test]
    fn missing_focus_fields_keep_every_pane_visible() {
        let mut value = snapshot_value();
        value.as_object_mut().unwrap().remove("focused_workspace_id");
        assert_eq!(
            visible_local_panes(&snapshot(value)),
            ids(&["w1:p1", "w1:p2", "w1:p3", "w2:p1", "w2:p2"])
        );
    }

    #[test]
    fn a_lost_link_stops_every_streamer_without_grace() {
        let now = Instant::now();
        let tracked = [
            PaneStreamState { local_id: "local-visible".into(), streaming: true, hidden_since: None },
            PaneStreamState {
                local_id: "local-hidden".into(),
                streaming: true,
                hidden_since: Some(now - Duration::from_secs(1)),
            },
        ];
        assert_eq!(
            plan_streams(&policy(), false, &ids(&["local-visible"]), &tracked, now),
            vec![StreamAction::Stop("local-visible".into()), StreamAction::Stop("local-hidden".into())]
        );
    }

    #[test]
    fn visible_only_off_never_stops_a_streamer() {
        let now = Instant::now();
        let policy = StreamPolicy { visible_only: false, pause_after: Duration::from_secs(30) };
        let tracked = [
            PaneStreamState {
                local_id: "local-hidden-streaming".into(),
                streaming: true,
                hidden_since: Some(now - Duration::from_secs(31)),
            },
            PaneStreamState { local_id: "local-paused".into(), streaming: false, hidden_since: None },
        ];
        assert_eq!(
            plan_streams(&policy, true, &HashSet::new(), &tracked, now),
            vec![StreamAction::Start("local-paused".into())]
        );
    }
}
