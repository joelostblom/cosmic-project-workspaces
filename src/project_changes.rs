use crate::layout::ProjectPosition;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Project,
    Workspace,
    Layout,
}

#[derive(Default)]
pub struct ProjectChanges {
    active: Option<(u64, Option<ProjectPosition>)>,
    pending: Option<Change>,
}

impl ProjectChanges {
    pub fn observe(&mut self, active: Option<(u64, Option<ProjectPosition>)>) {
        match (&self.active, &active) {
            (Some((previous, before)), Some((current, after))) => {
                let changed_project =
                    before.as_ref().map(|p| &p.name) != after.as_ref().map(|p| &p.name);
                if previous != current {
                    self.pending = Some(if after.is_none() {
                        Change::Layout
                    } else if changed_project || self.pending == Some(Change::Project) {
                        // Keep a project entry queued if a newer row arrives in the same batch.
                        Change::Project
                    } else {
                        Change::Workspace
                    });
                } else if changed_project {
                    // Reassignment or a rename doesn't announce a switch.
                    self.pending = Some(Change::Layout);
                } else if before != after {
                    self.pending.get_or_insert(Change::Layout);
                }
            }
            (Some(_), None) => self.pending = Some(Change::Layout),
            _ => {}
        }
        self.active = active;
    }

    pub fn take(&mut self) -> Option<(Change, Option<&ProjectPosition>)> {
        let change = self.pending.take()?;
        let position = self
            .active
            .as_ref()
            .and_then(|(_, position)| position.as_ref());
        Some((change, position))
    }

    pub fn announce_move(&mut self, workspace: u64) {
        // Explicit transfers enter a different project without changing the
        // active workspace. Other assignment edits should still be quiet.
        if let Some((active, Some(_))) = &self.active
            && *active == workspace
        {
            self.pending = Some(Change::Project);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(
        serial: u64,
        name: &str,
        row: usize,
        total: usize,
    ) -> Option<(u64, Option<ProjectPosition>)> {
        Some((
            serial,
            Some(ProjectPosition {
                name: name.into(),
                row,
                total,
            }),
        ))
    }

    fn assert_position(changes: &mut ProjectChanges, kind: Change, expected: (&str, usize, usize)) {
        let (change, position) = changes.take().expect("expected an overlay update");
        let p = position.expect("expected an assigned workspace");
        assert_eq!(change, kind);
        assert_eq!((p.name.as_str(), p.row, p.total), expected);
    }

    #[test]
    fn distinguishes_project_entries_from_workspace_switches_and_layout_edits() {
        let mut changes = ProjectChanges::default();
        changes.observe(at(1, "Work", 1, 2));
        assert!(changes.take().is_none()); // Initial snapshot.
        changes.observe(at(2, "Work", 2, 2));
        assert_position(&mut changes, Change::Workspace, ("Work", 2, 2));
        changes.observe(at(2, "Work", 1, 1)); // Layout shrank in place.
        assert_position(&mut changes, Change::Layout, ("Work", 1, 1));
        changes.observe(at(2, "Work", 1, 1));
        assert!(changes.take().is_none()); // No actual change.
        changes.observe(at(3, "Personal", 2, 3));
        assert_position(&mut changes, Change::Project, ("Personal", 2, 3));
        changes.observe(at(3, "Hobby", 1, 1));
        assert_position(&mut changes, Change::Layout, ("Hobby", 1, 1));
        changes.observe(Some((4, None)));
        assert!(matches!(changes.take(), Some((Change::Layout, None))));
        changes.observe(at(1, "Work", 1, 2));
        assert_position(&mut changes, Change::Project, ("Work", 1, 2));
    }

    #[test]
    fn coalesces_switches_without_leaving_a_stale_banner() {
        let mut changes = ProjectChanges::default();
        changes.observe(at(1, "Work", 1, 1));
        changes.observe(at(2, "Personal", 1, 1));
        changes.observe(at(3, "Hobby", 1, 2));
        changes.observe(at(5, "Hobby", 2, 2));
        assert_position(&mut changes, Change::Project, ("Hobby", 2, 2));
        changes.observe(at(2, "Personal", 1, 1));
        changes.observe(Some((4, None)));
        assert!(matches!(changes.take(), Some((Change::Layout, None))));
    }

    #[test]
    fn a_transfer_announces_the_destination_without_switching_workspaces() {
        let mut changes = ProjectChanges::default();
        changes.observe(at(1, "Work", 1, 1));
        changes.observe(at(1, "Personal", 2, 3));
        assert!(matches!(changes.take(), Some((Change::Layout, _))));
        changes.announce_move(2);
        assert!(changes.take().is_none());
        changes.announce_move(1);
        assert_position(&mut changes, Change::Project, ("Personal", 2, 3));
        assert!(changes.take().is_none());
    }
}
