// Pure layout reconciliation between a remote tab's split tree and its mirror.
//
// Split geometry is the one part of a mirror that can't be copied verbatim: the
// local tree is built by replaying splits, and herdr's only in-place primitives
// are `pane.split` (splits a LEAF, new pane always lands second),
// `pane.swap` (exchanges two panes, shape and ratios untouched) and
// `layout.set_split_ratio`. `pane.move` can't help: a move whose destination is
// the pane's own tab returns unchanged (`PaneMoveReason::SameTab`), so a tab's
// tree can never be re-parented after the fact. Everything here is therefore
// built around getting the shape right AT PLACEMENT TIME and keeping it right.
//
// All of it is pure: `plan_placements` says where to split, `plan_sync` says
// what to swap and which ratios to correct on which side. mirror.rs does the
// I/O. That split is deliberate — the interesting cases (nesting, inverted
// order, both sides resized at once) are exactly the ones that are miserable to
// exercise against a live remote.

use std::collections::{BTreeMap, BTreeSet};

use crate::mirror::LayoutNode;

/// Ratio drift below this is ignored: both sides derive ratios from independent
/// cell-grid math, and herdr stores them as f32 (a remote resize reports
/// 0.45000002), so sub-percent wobble is not a real desync.
pub const RATIO_EPSILON: f64 = 0.01;

pub fn ratios_equal(a: f64, b: f64) -> bool {
    (a - b).abs() <= RATIO_EPSILON
}

/// Where to mirror one not-yet-mirrored remote pane.
///
/// `target` is a REMOTE pane id whose mirror already exists; the caller maps it
/// through its own pane table. `ratio` is the remote split's ratio verbatim,
/// which is correct in both orders: the local split is created as
/// `first = target, second = new`, and when the remote has them the other way
/// round `swap` exchanges the two panes afterwards, which preserves the split's
/// ratio and so lands the same geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub pane: String,
    pub target: String,
    pub direction: String,
    pub ratio: f64,
    pub swap: bool,
}

fn leaf_id(node: &LayoutNode) -> Option<&str> {
    match node {
        LayoutNode::Pane { pane_id, .. } => pane_id.as_deref().filter(|p| !p.is_empty()),
        LayoutNode::Split { .. } => None,
    }
}

pub fn pane_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Pane { pane_id, .. } => {
            if let Some(p) = pane_id.as_deref().filter(|p| !p.is_empty()) {
                out.push(p.to_string());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            pane_ids(first, out);
            pane_ids(second, out);
        }
    }
}

/// The chain of splits from the root down to `pane`, each paired with which
/// child the descent took (`true` = second).
fn path_to<'a>(node: &'a LayoutNode, pane: &str, acc: &mut Vec<(&'a LayoutNode, bool)>) -> bool {
    match node {
        LayoutNode::Pane { .. } => leaf_id(node) == Some(pane),
        LayoutNode::Split { first, second, .. } => {
            acc.push((node, false));
            if path_to(first, pane, acc) {
                return true;
            }
            acc.pop();
            acc.push((node, true));
            if path_to(second, pane, acc) {
                return true;
            }
            acc.pop();
            false
        }
    }
}

fn mirrored_in(node: &LayoutNode, mirrored: &BTreeSet<String>) -> Vec<String> {
    let mut all = Vec::new();
    pane_ids(node, &mut all);
    all.into_iter().filter(|p| mirrored.contains(p)).collect()
}

/// Where one pane attaches to the mirror as it stands.
///
/// The local tree is the remote tree with unmirrored panes pruned, so placing
/// `pane` means finding the split where its branch leaves the already-mirrored
/// part: the DEEPEST ancestor whose other side holds a mirrored pane. Splits
/// below that one describe panes that don't exist locally yet, and splits above
/// it separate `pane` from the wrong neighbour.
/// Returns the placement and the attach split's depth (0 = root), which orders
/// the reconstruction: a tree has to be rebuilt outside-in, and a pane placed
/// out of turn can strand a shallower one against a subtree it can no longer
/// split.
fn attach_point(
    remote: &LayoutNode,
    pane: &str,
    mirrored: &BTreeSet<String>,
) -> Option<Result<(Placement, usize), ()>> {
    let mut chain = Vec::new();
    if !path_to(remote, pane, &mut chain) {
        return None;
    }
    for (depth, (split, went_second)) in chain.iter().enumerate().rev() {
        let LayoutNode::Split { direction, ratio, first, second } = split else { continue };
        let other = if *went_second { first } else { second };
        let neighbours = mirrored_in(other, mirrored);
        match neighbours.len() {
            0 => continue, // that whole side is unmirrored: keep climbing
            1 => {
                return Some(Ok((
                    Placement {
                        pane: pane.to_string(),
                        target: neighbours[0].clone(),
                        direction: direction.to_string(),
                        ratio: *ratio,
                        swap: !*went_second,
                    },
                    depth,
                )))
            }
            // the neighbouring side is a multi-pane subtree, and pane.split
            // splits a leaf: nothing can wrap a subtree in a new split
            _ => return Some(Err(())),
        }
    }
    Some(Err(()))
}

/// Ordered placements for every pane in `remote` that isn't mirrored yet, plus
/// the ones that can't be placed faithfully.
///
/// A placement is only emitted when the new pane's remote sibling is a single
/// pane that is already mirrored, because `pane.split` splits a leaf: there is
/// no primitive that wraps a whole subtree in a new split. That is not as
/// limiting as it sounds, since the remote grew the same way — herdr's own
/// split replaces a leaf with `Split { leaf, new }` — so a pane's sibling is a
/// lone leaf at the moment it is created. Panes are emitted in dependency
/// order, which is what makes a burst of several new remote panes (a converge
/// that fell behind, or a whole tab of them) reproduce exactly instead of
/// flattening: each split lands against a sibling the previous step just made.
///
/// Anything left over (a remote `pane.move`/`swap` reshaped the tree, so a new
/// pane's sibling is a multi-pane subtree) comes back in the second return
/// value for the caller to place by its own fallback and log, rather than being
/// silently mis-shaped.
pub fn plan_placements(
    remote: &LayoutNode,
    mirrored: &BTreeSet<String>,
) -> (Vec<Placement>, Vec<String>) {
    let mut all = Vec::new();
    pane_ids(remote, &mut all);
    let mut pending: Vec<String> = all.into_iter().filter(|p| !mirrored.contains(p)).collect();
    let mut placed: BTreeSet<String> = mirrored.clone();
    let mut out = Vec::new();

    // Fixpoint, shallowest attach point first: placing one pane can unblock the
    // next (a pane whose only neighbour was itself new), and going outside-in is
    // what keeps a burst from stranding a pane whose neighbouring side has
    // meanwhile grown into a subtree.
    loop {
        let next = pending
            .iter()
            .enumerate()
            .filter_map(|(i, pane)| match attach_point(remote, pane, &placed) {
                Some(Ok((_, depth))) => Some((depth, i)),
                _ => None,
            })
            .min();
        let Some((_, i)) = next else { break };
        let pane = pending.remove(i);
        let Some(Ok((placement, _))) = attach_point(remote, &pane, &placed) else { continue };
        out.push(placement);
        placed.insert(pane);
    }
    (out, pending)
}

/// Which side a ratio correction is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RatioFix {
    pub path: Vec<bool>,
    pub ratio: f64,
    pub apply_to: Side,
}

#[derive(Debug, Default, PartialEq)]
pub struct SyncPlan {
    /// pairs of LOCAL pane ids to `pane.swap`, in order
    pub swaps: Vec<(String, String)>,
    pub ratios: Vec<RatioFix>,
    /// path key -> the ratio both sides agree on after this plan is applied.
    /// Persisted by the caller and fed back in as `base`, which is what makes
    /// the sync two-way: without a base you can see that the sides differ but
    /// not which one moved.
    pub base: BTreeMap<String, f64>,
    /// the trees disagree in shape, so nothing below the divergence is
    /// comparable. Placement, not ratio sync, is what fixes this.
    pub structural_mismatch: bool,
}

/// Stable key for a split path. `""` is the root; `F`/`T` descend into the
/// first/second child, matching `layout.set_split_ratio`'s bool array.
pub fn path_key(path: &[bool]) -> String {
    path.iter().map(|b| if *b { 'T' } else { 'F' }).collect()
}

/// Reconcile one tab's geometry.
///
/// Ratios are a three-way merge against `base` (the last value both sides
/// agreed on), so an edit propagates in whichever direction it was actually
/// made: resize on the remote and the mirror follows, resize the mirror and the
/// remote follows. Comparing the two sides alone can only say "these differ",
/// which is why the naive version has to pick a permanent winner and revert the
/// other side's resize. When both moved since `base`, `local_wins` breaks the
/// tie: true for a host this daemon drives (the remote is headless, the local
/// window is the only one anybody looks at), false for a watch-only host whose
/// own layout is authoritative.
///
/// Pane identity is checked too, not just shape: a remote `pane.swap` leaves
/// both trees the same shape with the panes exchanged, which a shape-only walk
/// would happily "fix" by resizing, moving the wrong pane. Those come back as
/// `swaps` (local pane ids) and must be applied before the ratios.
pub fn plan_sync(
    remote: &LayoutNode,
    local: &LayoutNode,
    map: &BTreeMap<String, String>,
    base: &BTreeMap<String, f64>,
    local_wins: bool,
) -> SyncPlan {
    let mut plan = SyncPlan::default();
    // (pane sitting here, pane that should sit here), in traversal order
    let mut seats: Vec<(String, String)> = Vec::new();
    walk(remote, local, &mut Vec::new(), map, base, local_wins, &mut plan, &mut seats);
    if !plan.structural_mismatch {
        plan.swaps = plan_swaps(seats);
    }
    plan
}

#[allow(clippy::too_many_arguments)] // one walk, all of it needed per level
fn walk(
    remote: &LayoutNode,
    local: &LayoutNode,
    path: &mut Vec<bool>,
    map: &BTreeMap<String, String>,
    base: &BTreeMap<String, f64>,
    local_wins: bool,
    plan: &mut SyncPlan,
    seats: &mut Vec<(String, String)>,
) {
    match (remote, local) {
        (
            LayoutNode::Split {
                direction: rdir,
                ratio: rratio,
                first: rfirst,
                second: rsecond,
            },
            LayoutNode::Split {
                direction: ldir,
                ratio: lratio,
                first: lfirst,
                second: lsecond,
            },
        ) => {
            if rdir != ldir {
                // same shape, different axis: a ratio here would resize the
                // wrong dimension
                plan.structural_mismatch = true;
                return;
            }
            let key = path_key(path);
            let agreed = decide(*rratio, *lratio, base.get(&key).copied(), local_wins);
            if let Some(side) = agreed.apply {
                plan.ratios.push(RatioFix { path: path.clone(), ratio: agreed.ratio, apply_to: side });
            }
            plan.base.insert(key, agreed.ratio);

            path.push(false);
            walk(rfirst, lfirst, path, map, base, local_wins, plan, seats);
            path.pop();
            path.push(true);
            walk(rsecond, lsecond, path, map, base, local_wins, plan, seats);
            path.pop();
        }
        (LayoutNode::Pane { .. }, LayoutNode::Pane { .. }) => {
            let (Some(rid), Some(lid)) = (leaf_id(remote), leaf_id(local)) else {
                plan.structural_mismatch = true;
                return;
            };
            // a remote pane with no mirror yet: shape isn't converged, so
            // ratios below here mean nothing
            let Some(want) = map.get(rid) else {
                plan.structural_mismatch = true;
                return;
            };
            seats.push((lid.to_string(), want.clone()));
        }
        _ => plan.structural_mismatch = true,
    }
}

struct Decision {
    ratio: f64,
    apply: Option<Side>,
}

/// Three-way merge of one split's ratio.
fn decide(remote: f64, local: f64, base: Option<f64>, local_wins: bool) -> Decision {
    if ratios_equal(remote, local) {
        return Decision { ratio: remote, apply: None };
    }
    let Some(base) = base else {
        // never synced: adopt the remote's geometry, which is the mirror's
        // starting point in either mode
        return Decision { ratio: remote, apply: Some(Side::Local) };
    };
    let local_moved = !ratios_equal(local, base);
    let remote_moved = !ratios_equal(remote, base);
    match (remote_moved, local_moved) {
        (true, false) => Decision { ratio: remote, apply: Some(Side::Local) },
        (false, true) => Decision { ratio: local, apply: Some(Side::Remote) },
        // both moved since the last agreement, or neither did while the sides
        // still differ (a write we never saw land): the configured owner wins
        _ if local_wins => Decision { ratio: local, apply: Some(Side::Remote) },
        _ => Decision { ratio: remote, apply: Some(Side::Local) },
    }
}

/// Turn "pane X sits where pane Y should" into `pane.swap` pairs.
///
/// Decomposing the permutation this way costs at most n-1 swaps and, unlike
/// emitting one swap per wrong seat, never undoes its own work.
fn plan_swaps(seats: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut have: Vec<String> = seats.iter().map(|s| s.0.clone()).collect();
    let want: Vec<String> = seats.iter().map(|s| s.1.clone()).collect();
    let mut out = Vec::new();
    for i in 0..have.len() {
        if have[i] == want[i] {
            continue;
        }
        // the wanted pane may be absent entirely (mid-converge); leave the seat
        // alone rather than inventing a swap
        if let Some(j) = (i + 1..have.len()).find(|&j| have[j] == want[i]) {
            out.push((have[i].clone(), have[j].clone()));
            have.swap(i, j);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str) -> LayoutNode {
        LayoutNode::Pane { pane_id: Some(id.into()), label: None }
    }

    fn split(direction: &str, ratio: f64, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            direction: direction.into(),
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn mirrored(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(r, l)| (r.to_string(), l.to_string())).collect()
    }

    fn base(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// The ordinary case: split a remote pane, mirror splits its sibling with
    /// the same direction and ratio, new pane second on both sides.
    #[test]
    fn places_a_new_pane_against_its_mirrored_sibling() {
        let remote = split("right", 0.3, leaf("p1"), leaf("p2"));
        let (places, stuck) = plan_placements(&remote, &mirrored(&["p1"]));
        assert!(stuck.is_empty());
        assert_eq!(
            places,
            vec![Placement {
                pane: "p2".into(),
                target: "p1".into(),
                direction: "right".into(),
                ratio: 0.3,
                swap: false,
            }]
        );
    }

    /// The remote has the new pane as the split's FIRST child (it was created
    /// before its sibling, or a remote swap reordered them). `pane.split`
    /// always puts the new pane second, so the placement asks for a swap
    /// afterwards; the ratio still travels verbatim because `pane.swap` leaves
    /// the split's ratio where it is.
    #[test]
    fn inverted_order_asks_for_a_swap_not_an_inverted_ratio() {
        let remote = split("down", 0.35, leaf("new"), leaf("old"));
        let (places, stuck) = plan_placements(&remote, &mirrored(&["old"]));
        assert!(stuck.is_empty());
        assert_eq!(
            places,
            vec![Placement {
                pane: "new".into(),
                target: "old".into(),
                direction: "down".into(),
                ratio: 0.35,
                swap: true,
            }]
        );
    }

    /// Several new panes at once (a converge that fell behind) must be ordered
    /// so each split lands against a sibling that already exists, which is what
    /// reproduces nesting instead of flattening every pane onto one target.
    #[test]
    fn orders_a_burst_so_nesting_is_reproduced() {
        // p1 | (p2 / p3), with only p1 mirrored: p2 must be placed before p3,
        // and p3 against p2 — not against p1
        let remote = split("right", 0.5, leaf("p1"), split("down", 0.4, leaf("p2"), leaf("p3")));
        let (places, stuck) = plan_placements(&remote, &mirrored(&["p1"]));
        assert!(stuck.is_empty());
        assert_eq!(places.len(), 2);
        assert_eq!((places[0].pane.as_str(), places[0].target.as_str()), ("p2", "p1"));
        assert_eq!(places[0].ratio, 0.5);
        assert_eq!((places[1].pane.as_str(), places[1].target.as_str()), ("p3", "p2"));
        assert_eq!(places[1].direction, "down");
        assert_eq!(places[1].ratio, 0.4);
    }

    /// Four panes, one mirrored: the reconstruction has to go outside-in. Taking
    /// panes in tree order instead places p2 beside p1 first, which turns the
    /// root's other side into a two-pane subtree and strands p3 for good.
    #[test]
    fn rebuilds_a_deep_tree_outside_in() {
        let remote = split(
            "down",
            0.5,
            split("right", 0.3, leaf("p1"), leaf("p2")),
            split("right", 0.6, leaf("p3"), leaf("p4")),
        );
        let (places, stuck) = plan_placements(&remote, &mirrored(&["p1"]));
        assert!(stuck.is_empty(), "stranded {stuck:?}");
        let steps: Vec<(&str, &str, &str, f64, bool)> = places
            .iter()
            .map(|p| (p.pane.as_str(), p.target.as_str(), p.direction.as_str(), p.ratio, p.swap))
            .collect();
        assert_eq!(
            steps,
            vec![
                // p3 branches off at the root, so it goes first
                ("p3", "p1", "down", 0.5, false),
                ("p2", "p1", "right", 0.3, false),
                ("p4", "p3", "right", 0.6, false),
            ]
        );
    }

    /// A new pane whose remote sibling is a multi-pane SUBTREE can't be
    /// reproduced: splitting a leaf can't wrap a subtree. Report it instead of
    /// applying an outer split's ratio to a brand-new inner split.
    #[test]
    fn reports_panes_it_cannot_place_faithfully() {
        // (p1|p2) | p3 with p1,p2 mirrored: p3's sibling is the (p1|p2) subtree
        let remote = split("right", 0.6, split("down", 0.5, leaf("p1"), leaf("p2")), leaf("p3"));
        let (places, stuck) = plan_placements(&remote, &mirrored(&["p1", "p2"]));
        assert!(places.is_empty());
        assert_eq!(stuck, vec!["p3".to_string()]);
    }

    #[test]
    fn nothing_to_place_when_every_pane_is_mirrored() {
        let remote = split("right", 0.5, leaf("p1"), leaf("p2"));
        let (places, stuck) = plan_placements(&remote, &mirrored(&["p1", "p2"]));
        assert!(places.is_empty() && stuck.is_empty());
    }

    /// First sync of a tab: no base yet, so the mirror adopts the remote's
    /// geometry regardless of which side owns the layout.
    #[test]
    fn first_sync_adopts_the_remote_ratio() {
        let remote = split("right", 0.3, leaf("p1"), leaf("p2"));
        let local = split("right", 0.5, leaf("l1"), leaf("l2"));
        let m = map(&[("p1", "l1"), ("p2", "l2")]);
        for local_wins in [false, true] {
            let plan = plan_sync(&remote, &local, &m, &BTreeMap::new(), local_wins);
            assert_eq!(
                plan.ratios,
                vec![RatioFix { path: vec![], ratio: 0.3, apply_to: Side::Local }]
            );
            assert_eq!(plan.base.get(""), Some(&0.3));
            assert!(plan.swaps.is_empty() && !plan.structural_mismatch);
        }
    }

    /// The remote was resized since the last agreement: the mirror follows,
    /// even on a host where local edits win ties.
    #[test]
    fn remote_resize_flows_to_the_mirror() {
        let remote = split("right", 0.3, leaf("p1"), leaf("p2"));
        let local = split("right", 0.5, leaf("l1"), leaf("l2"));
        let plan = plan_sync(
            &remote,
            &local,
            &map(&[("p1", "l1"), ("p2", "l2")]),
            &base(&[("", 0.5)]),
            true,
        );
        assert_eq!(plan.ratios, vec![RatioFix { path: vec![], ratio: 0.3, apply_to: Side::Local }]);
    }

    /// The mirror was resized since the last agreement: the REMOTE follows.
    /// This is the direction the one-way version can't express — it reverts the
    /// local resize on the next pass instead.
    #[test]
    fn local_resize_flows_to_the_remote() {
        let remote = split("right", 0.5, leaf("p1"), leaf("p2"));
        let local = split("right", 0.7, leaf("l1"), leaf("l2"));
        let plan = plan_sync(
            &remote,
            &local,
            &map(&[("p1", "l1"), ("p2", "l2")]),
            &base(&[("", 0.5)]),
            false, // even a watch-only host: nobody else moved it
        );
        assert_eq!(plan.ratios, vec![RatioFix { path: vec![], ratio: 0.7, apply_to: Side::Remote }]);
        assert_eq!(plan.base.get(""), Some(&0.7));
    }

    /// Both sides moved since the last agreement. Unresolvable on the evidence,
    /// so the configured owner wins: the local window for a driven headless
    /// remote, the remote for a machine with its own display.
    #[test]
    fn simultaneous_resizes_go_to_the_configured_owner() {
        let remote = split("right", 0.4, leaf("p1"), leaf("p2"));
        let local = split("right", 0.7, leaf("l1"), leaf("l2"));
        let m = map(&[("p1", "l1"), ("p2", "l2")]);
        let b = base(&[("", 0.5)]);

        let drive = plan_sync(&remote, &local, &m, &b, true);
        assert_eq!(drive.ratios, vec![RatioFix { path: vec![], ratio: 0.7, apply_to: Side::Remote }]);

        let watch = plan_sync(&remote, &local, &m, &b, false);
        assert_eq!(watch.ratios, vec![RatioFix { path: vec![], ratio: 0.4, apply_to: Side::Local }]);
    }

    /// In sync: no writes, and the agreement is recorded so the next pass can
    /// still tell which side moved.
    #[test]
    fn agreement_records_a_base_without_writing() {
        let remote = split("right", 0.3, leaf("p1"), split("down", 0.25, leaf("p2"), leaf("p3")));
        let local = split("right", 0.3, leaf("l1"), split("down", 0.25, leaf("l2"), leaf("l3")));
        let plan = plan_sync(
            &remote,
            &local,
            &map(&[("p1", "l1"), ("p2", "l2"), ("p3", "l3")]),
            &BTreeMap::new(),
            false,
        );
        assert!(plan.ratios.is_empty() && plan.swaps.is_empty());
        assert_eq!(plan.base.get(""), Some(&0.3));
        assert_eq!(plan.base.get("T"), Some(&0.25));
    }

    /// Sub-percent wobble is not a desync: herdr stores ratios as f32, so a
    /// remote resize reports values like 0.45000002.
    #[test]
    fn sub_epsilon_wobble_is_not_a_correction() {
        let remote = split("right", 0.45000002, leaf("p1"), leaf("p2"));
        let local = split("right", 0.45, leaf("l1"), leaf("l2"));
        let plan =
            plan_sync(&remote, &local, &map(&[("p1", "l1"), ("p2", "l2")]), &BTreeMap::new(), false);
        assert!(plan.ratios.is_empty());
    }

    /// Nested splits at depth, addressed by the bool path
    /// `layout.set_split_ratio` expects.
    #[test]
    fn corrects_nested_splits_by_path() {
        let remote = split(
            "right",
            0.5,
            leaf("p1"),
            split("down", 0.7, leaf("p2"), split("right", 0.2, leaf("p3"), leaf("p4"))),
        );
        let local = split(
            "right",
            0.5,
            leaf("l1"),
            split("down", 0.4, leaf("l2"), split("right", 0.2, leaf("l3"), leaf("l4"))),
        );
        let plan = plan_sync(
            &remote,
            &local,
            &map(&[("p1", "l1"), ("p2", "l2"), ("p3", "l3"), ("p4", "l4")]),
            &BTreeMap::new(),
            false,
        );
        assert_eq!(
            plan.ratios,
            vec![RatioFix { path: vec![true], ratio: 0.7, apply_to: Side::Local }]
        );
        assert_eq!(plan.base.get("TT"), Some(&0.2));
    }

    /// A remote swap leaves both trees the same shape with the panes
    /// exchanged. Resizing would move the wrong pane, so this must come back
    /// as a swap.
    #[test]
    fn exchanged_panes_come_back_as_swaps() {
        let remote = split("right", 0.5, leaf("p2"), leaf("p1"));
        let local = split("right", 0.5, leaf("l1"), leaf("l2"));
        let plan =
            plan_sync(&remote, &local, &map(&[("p1", "l1"), ("p2", "l2")]), &BTreeMap::new(), false);
        assert_eq!(plan.swaps, vec![("l1".to_string(), "l2".to_string())]);
        assert!(plan.ratios.is_empty() && !plan.structural_mismatch);
    }

    /// Three panes rotated: one swap per displaced pair, never more than n-1,
    /// and no swap that undoes an earlier one.
    #[test]
    fn rotated_panes_decompose_into_minimal_swaps() {
        let remote = split("right", 0.5, leaf("p2"), split("down", 0.5, leaf("p3"), leaf("p1")));
        let local = split("right", 0.5, leaf("l1"), split("down", 0.5, leaf("l2"), leaf("l3")));
        let plan = plan_sync(
            &remote,
            &local,
            &map(&[("p1", "l1"), ("p2", "l2"), ("p3", "l3")]),
            &BTreeMap::new(),
            false,
        );
        assert_eq!(
            plan.swaps,
            vec![("l1".to_string(), "l2".to_string()), ("l1".to_string(), "l3".to_string())]
        );
    }

    /// Shapes disagree: stop. Placement fixes this, and a ratio applied across
    /// a divergence resizes something unrelated.
    #[test]
    fn structural_mismatch_blocks_everything() {
        let remote = split("right", 0.3, leaf("p1"), split("down", 0.5, leaf("p2"), leaf("p3")));
        let local = split("right", 0.5, leaf("l1"), leaf("l2"));
        let plan = plan_sync(
            &remote,
            &local,
            &map(&[("p1", "l1"), ("p2", "l2"), ("p3", "l3")]),
            &BTreeMap::new(),
            false,
        );
        assert!(plan.structural_mismatch);
        assert!(plan.swaps.is_empty());
    }

    /// Same shape, different axis: a ratio here would resize the wrong
    /// dimension, so it counts as a structural mismatch.
    #[test]
    fn direction_mismatch_is_structural() {
        let remote = split("right", 0.3, leaf("p1"), leaf("p2"));
        let local = split("down", 0.5, leaf("l1"), leaf("l2"));
        let plan =
            plan_sync(&remote, &local, &map(&[("p1", "l1"), ("p2", "l2")]), &BTreeMap::new(), false);
        assert!(plan.structural_mismatch);
        assert!(plan.ratios.is_empty());
    }

    /// A remote pane with no mirror yet means the shape isn't converged, so
    /// ratios are not yet meaningful.
    #[test]
    fn unmirrored_pane_is_structural() {
        let remote = split("right", 0.3, leaf("p1"), leaf("p2"));
        let local = split("right", 0.5, leaf("l1"), leaf("l2"));
        let plan = plan_sync(&remote, &local, &map(&[("p1", "l1")]), &BTreeMap::new(), false);
        assert!(plan.structural_mismatch);
    }
}
