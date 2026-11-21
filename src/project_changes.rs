use crate::layout::ProjectPosition;

#[derive(Default)]
pub struct ProjectChanges {
    active: Option<(u64, Option<ProjectPosition>)>,
    pending: Option<String>,
}

impl ProjectChanges {
    pub fn observe(&mut self, active: Option<(u64, Option<ProjectPosition>)>) {
        match (&self.active, &active) {
            (Some((previous, before)), Some((current, after))) => {
                let before = before.as_ref().map(|position| &position.name);
                let after_name = after.as_ref().map(|position| &position.name);
                if before != after_name {
                    // Edits can change the project mapping without changing desktops.
                    self.pending = if previous != current {
                        after.as_ref().map(ProjectPosition::title)
                    } else {
                        None
                    };
                } else if self.pending.is_some() {
                    // Carry the latest row into a banner already queued in this batch.
                    self.pending = after.as_ref().map(ProjectPosition::title);
                }
            }
            (_, None) => self.pending = None,
            _ => {}
        }
        self.active = active;
    }

    pub fn take(&mut self) -> Option<String> {
        self.pending.take()
    }

    pub fn announce_move(&mut self, workspace: u64) {
        // Explicit transfers enter a different project without changing the
        // active workspace. Other assignment edits should still be quiet.
        if let Some((active, Some(position))) = &self.active
            && *active == workspace
        {
            self.pending = Some(position.title());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProjectChanges, ProjectPosition};

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

    #[test]
    fn announces_only_entry_to_a_different_assigned_project() {
        let mut changes = ProjectChanges::default();
        changes.observe(at(1, "Work", 1, 2));
        assert_eq!(changes.take(), None); // Initial snapshot.
        changes.observe(at(2, "Work", 2, 2));
        assert_eq!(changes.take(), None); // A different row.
        changes.observe(at(3, "Personal", 2, 3));
        assert_eq!(changes.take().as_deref(), Some("Personal (2/3)"));
        changes.observe(at(3, "Hobby", 1, 1));
        assert_eq!(changes.take(), None); // An assignment edit.
        changes.observe(Some((4, None)));
        assert_eq!(changes.take(), None);
        changes.observe(at(1, "Work", 1, 2));
        assert_eq!(changes.take().as_deref(), Some("Work (1/2)"));
    }

    #[test]
    fn coalesces_switches_without_leaving_a_stale_banner() {
        let mut changes = ProjectChanges::default();
        changes.observe(at(1, "Work", 1, 1));
        changes.observe(at(2, "Personal", 1, 1));
        changes.observe(at(3, "Hobby", 1, 2));
        changes.observe(at(5, "Hobby", 2, 2));
        assert_eq!(changes.take().as_deref(), Some("Hobby (2/2)"));
        changes.observe(at(2, "Personal", 1, 1));
        changes.observe(Some((4, None)));
        assert_eq!(changes.take(), None);
    }

    #[test]
    fn a_transfer_announces_the_destination_without_switching_workspaces() {
        let mut changes = ProjectChanges::default();
        changes.observe(at(1, "Work", 1, 1));
        changes.observe(at(1, "Personal", 2, 3));
        assert_eq!(changes.take(), None);
        changes.announce_move(2);
        assert_eq!(changes.take(), None);
        changes.announce_move(1);
        assert_eq!(changes.take().as_deref(), Some("Personal (2/3)"));
        assert_eq!(changes.take(), None);
    }
}
