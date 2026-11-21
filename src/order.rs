use std::collections::HashSet;

pub struct OrderPlan {
    pub expected: Vec<u64>,
    /// (workspace to move, workspace it should immediately precede).
    pub moves: Vec<(u64, u64)>,
}

impl OrderPlan {
    pub fn new(current: &[u64], project_order: &[u64]) -> Self {
        let mut expected = current.to_vec();
        let mut moves = Vec::new();
        let managed: HashSet<_> = project_order.iter().copied().collect();
        if let Some(start) = current.iter().position(|id| managed.contains(id)) {
            // Start at the first assigned slot. Pull managed workspaces
            // together without moving any unassigned workspace directly.
            for (row, id) in project_order
                .iter()
                .filter(|id| current.contains(id))
                .enumerate()
            {
                let target = start + row;
                let from = expected[target..]
                    .iter()
                    .position(|workspace| workspace == id)
                    .expect("a live project workspace appears only once")
                    + target;
                if from != target {
                    moves.push((*id, expected[target]));
                    expected.remove(from);
                    expected.insert(target, *id);
                }
            }
        }
        Self { expected, moves }
    }
}

#[cfg(test)]
mod tests {
    use super::OrderPlan;

    #[test]
    fn gathers_project_rows_without_directly_moving_unassigned_workspaces() {
        let plan = OrderPlan::new(&[1, 2, 3, 4, 5, 6, 7], &[6, 4, 2, 5]);
        assert_eq!(plan.expected, [1, 6, 4, 2, 5, 3, 7]);
        assert_eq!(plan.moves, [(6, 2), (4, 2), (5, 3)]);
        assert!(
            OrderPlan::new(&plan.expected, &[6, 4, 2, 5])
                .moves
                .is_empty()
        );
    }

    #[test]
    fn skips_unavailable_members_and_leaves_a_single_assignment_in_place() {
        let plan = OrderPlan::new(&[1, 2, 3, 4], &[6, 2]);
        assert_eq!(plan.expected, [1, 2, 3, 4]);
        assert!(plan.moves.is_empty());
    }
}
