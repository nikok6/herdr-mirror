// Pane-to-slot allocation for ssh stream pooling (`ssh_streams_per_connection`
// in hosts.toml). A slot is one shared ssh ControlMaster connection; up to
// `capacity` pane streams may multiplex sessions onto it (see remote.rs's
// `stream_control_path` / `ensure_stream_master` for the sockets themselves).
//
// Deliberately not hash/modulo: that can't guarantee the per-slot capacity
// bound (two pane ids can hash to the same slot however many are already
// there), and it reshuffles every pane whenever the pane count changes.
// `allocate` instead keeps every still-valid assignment exactly where it is
// and only places panes that need a new home.

use std::collections::{BTreeMap, HashMap};

/// Assign every pane in `panes` to a slot in `[0, ceil(len/capacity))` so no
/// slot ends up with more than `capacity` panes.
///
/// `panes` maps each live pane id to its current slot, if any — `None` for a
/// pane that has never been assigned. Iteration order matters: callers pass a
/// `BTreeMap` so retained assignments are decided in the same deterministic
/// pane-id order on every call, which is what makes a converge pass's result
/// reproducible independent of remote event ordering.
///
/// Two passes: first keep every assignment that still fits under its slot's
/// capacity (processed in pane-id order, so if a slot is over capacity —
/// stale state after `capacity` shrank, or corrupt state with too many panes
/// crammed into one slot — only the first `capacity` panes by id keep it and
/// the rest fall through). Second, first-fit every still-unassigned pane into
/// the lowest-numbered slot with spare room, opening a new slot only once
/// every existing one is full. This is what fills capacity a removed pane
/// freed before growing the slot count, and what keeps a pure growth in
/// `capacity` (same panes, larger N) from moving anyone: every existing
/// assignment already fits and pass one keeps all of them.
pub fn allocate(panes: &BTreeMap<String, Option<u32>>, capacity: usize) -> BTreeMap<String, u32> {
    assert!(capacity >= 1, "stream pool capacity must be at least 1");
    let mut occupancy: HashMap<u32, usize> = HashMap::new();
    let mut result: BTreeMap<String, u32> = BTreeMap::new();
    let mut unassigned: Vec<&String> = Vec::new();

    // No legitimate allocation ever needs more slots than panes (worst case:
    // capacity 1, one pane per slot), so a persisted slot at or past this
    // bound is stale/corrupt — from a since-shrunk pane set, or hand-edited
    // state — and is reassigned rather than trusted.
    let max_valid_slot = panes.len() as u32;
    for (pane, slot) in panes {
        match slot {
            Some(s) if *s < max_valid_slot && *occupancy.get(s).unwrap_or(&0) < capacity => {
                *occupancy.entry(*s).or_insert(0) += 1;
                result.insert(pane.clone(), *s);
            }
            _ => unassigned.push(pane),
        }
    }

    let mut next_new_slot = occupancy.keys().max().map_or(0, |m| m + 1);
    for pane in unassigned {
        let slot = (0..next_new_slot)
            .find(|s| *occupancy.get(s).unwrap_or(&0) < capacity)
            .unwrap_or_else(|| {
                let s = next_new_slot;
                next_new_slot += 1;
                s
            });
        *occupancy.entry(slot).or_insert(0) += 1;
        result.insert(pane.clone(), slot);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(ids: &[&str]) -> BTreeMap<String, Option<u32>> {
        ids.iter().map(|s| (s.to_string(), None)).collect()
    }

    fn occupancy(assigned: &BTreeMap<String, u32>) -> BTreeMap<u32, usize> {
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for slot in assigned.values() {
            *counts.entry(*slot).or_insert(0) += 1;
        }
        counts
    }

    #[test]
    fn no_slot_exceeds_capacity() {
        let panes = fresh(&["p1", "p2", "p3", "p4", "p5", "p6", "p7"]);
        let assigned = allocate(&panes, 3);
        assert_eq!(assigned.len(), 7);
        for count in occupancy(&assigned).values() {
            assert!(*count <= 3, "{:?}", occupancy(&assigned));
        }
        // 7 panes at capacity 3 need 3 slots (3+3+1), not 7 (one per pane)
        assert_eq!(occupancy(&assigned).len(), 3);
    }

    /// Also the proof that capacity is a per-slot pane-count bound, not a
    /// slot count: capacity 2 still reaches slot index 2 (a 3rd slot) once
    /// a 5th pane needs one, since `ceil(5/2) = 3` slots are needed however
    /// small the configured capacity is (see `config::validate_unique_stream_namespaces`'s
    /// doc comment for why that distinction rules out slot enumeration
    /// there).
    #[test]
    fn fresh_panes_fill_the_lowest_slot_first() {
        let panes = fresh(&["p1", "p2", "p3", "p4", "p5"]);
        let assigned = allocate(&panes, 2);
        // BTreeMap iteration is pane-id order, so p1..p5 fill 0,0,1,1,2
        assert_eq!(assigned["p1"], 0);
        assert_eq!(assigned["p2"], 0);
        assert_eq!(assigned["p3"], 1);
        assert_eq!(assigned["p4"], 1);
        assert_eq!(assigned["p5"], 2);
    }

    /// A converge pass that changes nothing must reassign nothing — the whole
    /// point of persisting slots instead of recomputing hash/modulo.
    #[test]
    fn stable_assignments_are_retained_across_reconciliation() {
        let mut panes: BTreeMap<String, Option<u32>> = BTreeMap::new();
        panes.insert("p1".into(), None);
        panes.insert("p2".into(), None);
        panes.insert("p3".into(), None);
        let first = allocate(&panes, 2);

        let again: BTreeMap<String, Option<u32>> =
            first.iter().map(|(k, v)| (k.clone(), Some(*v))).collect();
        let second = allocate(&again, 2);
        assert_eq!(
            first, second,
            "an unchanged pane set must get the identical assignment back"
        );
    }

    /// Growing capacity must not move a single existing pane: every retained
    /// assignment already satisfies a larger cap, so pass one keeps all of
    /// them and nothing reaches the first-fit pass.
    #[test]
    fn growing_capacity_moves_nobody() {
        let panes = fresh(&["p1", "p2", "p3", "p4", "p5"]);
        let at_two = allocate(&panes, 2);

        let carried: BTreeMap<String, Option<u32>> =
            at_two.iter().map(|(k, v)| (k.clone(), Some(*v))).collect();
        let at_five = allocate(&carried, 5);
        assert_eq!(at_two, at_five, "growth must not rebalance survivors");
    }

    /// Removing a pane must free its slot for the next unassigned pane before
    /// any new slot opens.
    #[test]
    fn removal_frees_capacity_before_a_new_slot_opens() {
        let panes = fresh(&["p1", "p2", "p3", "p4"]);
        let assigned = allocate(&panes, 2); // p1,p2 -> 0; p3,p4 -> 1

        // p2 leaves, p5 arrives
        let mut next: BTreeMap<String, Option<u32>> = BTreeMap::new();
        next.insert("p1".into(), Some(assigned["p1"]));
        next.insert("p3".into(), Some(assigned["p3"]));
        next.insert("p4".into(), Some(assigned["p4"]));
        next.insert("p5".into(), None);
        let reassigned = allocate(&next, 2);

        assert_eq!(reassigned["p1"], assigned["p1"], "survivor must not move");
        assert_eq!(reassigned["p3"], assigned["p3"], "survivor must not move");
        assert_eq!(reassigned["p4"], assigned["p4"], "survivor must not move");
        assert_eq!(
            reassigned["p5"], assigned["p1"],
            "p5 must fill p2's freed seat, not slot 2"
        );
        assert_eq!(occupancy(&reassigned).len(), 2, "no new slot needed");
    }

    /// Shrinking N must move only the panes that no longer fit, never a
    /// pane that already satisfies the smaller cap.
    #[test]
    fn shrinking_capacity_moves_only_the_required_overflow() {
        let panes = fresh(&["p1", "p2", "p3", "p4"]);
        let at_four = allocate(&panes, 4); // all four in slot 0

        let carried: BTreeMap<String, Option<u32>> =
            at_four.iter().map(|(k, v)| (k.clone(), Some(*v))).collect();
        let at_two = allocate(&carried, 2);

        // pane-id order keeps the first two in slot 0; only the overflow moves
        assert_eq!(at_two["p1"], 0);
        assert_eq!(at_two["p2"], 0);
        assert_eq!(at_two["p3"], 1);
        assert_eq!(at_two["p4"], 1);
    }

    /// Stale/corrupt state can have more panes recorded against one slot than
    /// the current capacity allows (e.g. hand-edited state, or a capacity
    /// lowered while the daemon was stopped). Only the excess — by pane-id
    /// order — may move; the allocator must not treat the whole slot as
    /// invalid.
    #[test]
    fn over_capacity_state_only_bumps_the_excess() {
        let mut panes: BTreeMap<String, Option<u32>> = BTreeMap::new();
        panes.insert("p1".into(), Some(0));
        panes.insert("p2".into(), Some(0));
        panes.insert("p3".into(), Some(0));
        let assigned = allocate(&panes, 2);
        assert_eq!(assigned["p1"], 0);
        assert_eq!(assigned["p2"], 0);
        assert_ne!(assigned["p3"], 0, "p3 overflows slot 0's capacity of 2");
        assert_eq!(assigned["p3"], 1);
    }

    /// A stale slot number that no longer exists at all (e.g. left over from
    /// a much larger N) must not panic and must be treated as unassigned.
    #[test]
    fn a_wildly_out_of_range_stale_slot_is_reassigned() {
        let mut panes: BTreeMap<String, Option<u32>> = BTreeMap::new();
        panes.insert("p1".into(), Some(9999));
        let assigned = allocate(&panes, 2);
        assert_eq!(assigned["p1"], 0);
    }

    #[test]
    fn empty_input_yields_no_assignments() {
        assert!(allocate(&BTreeMap::new(), 4).is_empty());
    }

    #[test]
    fn capacity_of_one_gives_every_pane_its_own_slot() {
        let panes = fresh(&["p1", "p2", "p3"]);
        let assigned = allocate(&panes, 1);
        let mut slots: Vec<u32> = assigned.values().copied().collect();
        slots.sort_unstable();
        assert_eq!(slots, vec![0, 1, 2]);
    }
}
