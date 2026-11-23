use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Key {
    Stable(String),
    Session(u64),
}

impl Key {
    fn stable(&self) -> Option<&str> {
        match self {
            Self::Stable(id) => Some(id),
            Self::Session(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Destination {
    Workspace(Key),
    BorrowSpare { project: SpareProject, side: Side },
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpareProject {
    Existing(String),
    Temporary,
}

#[derive(Default)]
pub struct Layout {
    pub projects: Vec<Project>,
}

pub struct Project {
    pub name: String,
    pub members: Vec<Key>,
    pub last: Option<Key>,
    pub temporary: bool,
}

#[derive(PartialEq, Eq)]
pub struct ProjectPosition {
    pub name: String,
    pub row: usize,
    pub total: usize,
}

impl Project {
    fn empty(name: String, temporary: bool) -> Self {
        Self {
            name,
            members: Vec::new(),
            last: None,
            temporary,
        }
    }

    fn add_workspace(&mut self, key: Key, top: bool) {
        let row = if top { 0 } else { self.members.len() };
        self.members.insert(row, key.clone());
        self.last = Some(key);
    }

    fn entry(&self, available: impl Fn(&Key) -> bool) -> Option<&Key> {
        self.last
            .as_ref()
            .filter(|key| available(key))
            .or_else(|| self.members.iter().find(|key| available(key)))
    }

    fn destination(&self, side: Side, available: impl Fn(&Key) -> bool) -> Option<Destination> {
        if self.members.is_empty() {
            Some(Destination::BorrowSpare {
                project: SpareProject::Existing(self.name.clone()),
                side,
            })
        } else {
            self.entry(available).cloned().map(Destination::Workspace)
        }
    }
}

#[derive(Default, Deserialize, PartialEq, Serialize)]
pub struct SavedLayout {
    projects: Vec<SavedProject>,
}

#[derive(Deserialize, PartialEq, Serialize)]
struct SavedProject {
    name: String,
    members: Vec<String>,
    last: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    temporary: bool,
}

impl Layout {
    pub fn initial(default_project: &str, names: Vec<String>) -> Result<Self> {
        let names = if names.is_empty() {
            let other = if default_project == "Personal" {
                "Work"
            } else {
                "Personal"
            };
            vec![default_project.to_owned(), other.to_owned()]
        } else {
            ensure!(
                names.iter().any(|name| name == default_project),
                "initial --project names must include the default project {default_project:?}; you can change it with --default-project"
            );
            names
        };
        let mut layout = Self::default();
        for name in names {
            validate_name(&name)?;
            ensure!(
                layout.project_index(&name).is_none(),
                "duplicate initial project {name:?}"
            );
            layout.projects.push(Project::empty(name, false));
        }
        Ok(layout)
    }

    pub fn restore(saved: SavedLayout) -> Result<Self> {
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        for project in &saved.projects {
            validate_name(&project.name)?;
            ensure!(names.insert(&project.name), "duplicate project name");
            ensure!(
                project.members.iter().all(|id| ids.insert(id)),
                "each workspace must belong to exactly one project"
            );
            ensure!(
                project
                    .last
                    .as_ref()
                    .is_none_or(|id| project.members.contains(id)),
                "last-used workspace is not in its project"
            );
        }
        Ok(Self {
            projects: saved
                .projects
                .into_iter()
                .filter(|project| !project.temporary || !project.members.is_empty())
                .map(|project| Project {
                    name: project.name,
                    members: project.members.into_iter().map(Key::Stable).collect(),
                    last: project.last.map(Key::Stable),
                    temporary: project.temporary,
                })
                .collect(),
        })
    }

    pub fn snapshot(&self) -> SavedLayout {
        SavedLayout {
            projects: self
                .projects
                .iter()
                .filter_map(|project| {
                    let members: Vec<_> = project
                        .members
                        .iter()
                        .filter_map(|key| key.stable().map(str::to_owned))
                        .collect();
                    (!project.temporary || !members.is_empty()).then(|| SavedProject {
                        name: project.name.clone(),
                        members,
                        last: project
                            .last
                            .as_ref()
                            .and_then(Key::stable)
                            .map(str::to_owned),
                        temporary: project.temporary,
                    })
                })
                .collect(),
        }
    }

    fn position(&self, key: &Key) -> Option<(usize, usize)> {
        self.projects.iter().enumerate().find_map(|(i, project)| {
            project
                .members
                .iter()
                .position(|k| k == key)
                .map(|j| (i, j))
        })
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.position(key).is_some()
    }

    pub fn project_name(&self, key: &Key) -> Option<&str> {
        self.position(key)
            .map(|(column, _)| self.projects[column].name.as_str())
    }

    pub fn is_single_row_temporary(&self, key: &Key) -> bool {
        self.position(key).is_some_and(|(column, _)| {
            let project = &self.projects[column];
            project.temporary && project.members.len() == 1
        })
    }

    fn project_index(&self, name: &str) -> Option<usize> {
        self.projects
            .iter()
            .position(|project| project.name == name)
    }

    pub fn keep_named(&mut self, name: &str) {
        if let Some(index) = self.project_index(name) {
            self.projects[index].temporary = false;
        }
    }

    pub fn assign(&mut self, key: Key, name: String, top: bool) -> Result<()> {
        validate_name(&name)?;
        self.detach(&key);
        self.get_or_create_project(&name).add_workspace(key, top);
        self.drop_empty();
        Ok(())
    }

    pub fn assign_missing(&mut self, name: &str, keys: Vec<Key>) -> bool {
        let missing: Vec<_> = keys.into_iter().filter(|key| !self.contains(key)).collect();
        if missing.is_empty() {
            return false;
        }
        // Discovering workspaces is not the same as visiting them.
        let project = self.get_or_create_project(name);
        project.members.extend(missing);
        project.temporary = false;
        true
    }

    fn get_or_create_project(&mut self, name: &str) -> &mut Project {
        if let Some(index) = self.project_index(name) {
            &mut self.projects[index]
        } else {
            self.projects.push(Project::empty(name.to_owned(), false));
            self.projects.last_mut().unwrap()
        }
    }

    pub fn move_to_neighbor(
        &mut self,
        key: Key,
        side: Side,
        default_project: &str,
    ) -> Result<String> {
        let Some((column, _)) = self.position(&key) else {
            bail!("active workspace is unassigned; use `cpw assign PROJECT` first");
        };
        let adjacent = match side {
            Side::Left => column.checked_sub(1),
            Side::Right => (column + 1 < self.projects.len()).then_some(column + 1),
        };
        let Some(index) = adjacent else {
            let name = self.next_temporary_name(1, default_project);
            self.detach(&key);
            self.insert_temporary(key, side, name.clone());
            return Ok(name);
        };
        let destination = self.projects[index].name.clone();
        self.assign(key, destination.clone(), false)?;
        Ok(destination)
    }

    pub fn assign_spare(
        &mut self,
        key: Key,
        project: SpareProject,
        side: Side,
        default_project: &str,
    ) -> Result<String> {
        let name = match project {
            SpareProject::Existing(name) => {
                validate_name(&name)?;
                self.release_extension(&key);
                self.get_or_create_project(&name)
                    .add_workspace(key, side == Side::Left);
                name
            }
            SpareProject::Temporary => {
                // Only continuing on the same empty preview carries its number
                // forward. A fresh loan can recycle names from earlier previews.
                let first = if self.is_single_row_temporary(&key) {
                    self.project_name(&key)
                        .and_then(|name| name.strip_prefix("Project "))
                        .and_then(|number| number.parse::<u64>().ok())
                        .and_then(|number| number.checked_add(1))
                        .unwrap_or(1)
                } else {
                    1
                };
                let name = self.next_temporary_name(first, default_project);
                self.release_extension(&key);
                self.insert_temporary(key, side, name.clone());
                name
            }
        };
        Ok(name)
    }

    fn next_temporary_name(&self, first: u64, default_project: &str) -> String {
        (first..=u64::MAX)
            .chain(1..first)
            .map(|number| format!("Project {number}"))
            .find(|name| name != default_project && self.project_index(name).is_none())
            .expect("a finite project list cannot use all names")
    }

    fn insert_temporary(&mut self, key: Key, side: Side, name: String) {
        let index = match side {
            Side::Left => 0,
            Side::Right => self.projects.len(),
        };
        let mut project = Project::empty(name, true);
        project.add_workspace(key, false);
        self.projects.insert(index, project);
        self.drop_empty();
    }

    pub fn rename_project(&mut self, from: &str, to: String) -> Result<()> {
        validate_name(&to)?;
        let Some(index) = self.project_index(from) else {
            bail!("unknown project {from:?}");
        };
        ensure!(
            from == to || self.project_index(&to).is_none(),
            "project {to:?} already exists"
        );
        self.projects[index].name = to;
        self.projects[index].temporary = false;
        Ok(())
    }

    fn detach(&mut self, key: &Key) -> bool {
        let Some((i, j)) = self.position(key) else {
            return false;
        };
        let project = &mut self.projects[i];
        project.members.remove(j);
        if project.last.as_ref() == Some(key) {
            project.last = None;
        }
        true
    }

    fn drop_empty(&mut self) {
        self.projects
            .retain(|project| !project.temporary || !project.members.is_empty());
    }

    pub fn unassign(&mut self, key: &Key) -> bool {
        let detached = self.detach(key);
        self.drop_empty();
        detached
    }

    pub fn release_extension(&mut self, key: &Key) {
        if let Some((column, row)) = self.position(key) {
            let project = &mut self.projects[column];
            if project.last.as_ref() == Some(key) {
                // Don't lose the last-used position just because an empty
                // bottom row went away.
                project.last = row
                    .checked_sub(1)
                    .map(|previous| project.members[previous].clone());
            }
            self.unassign(key);
        }
    }

    pub fn forget(&mut self, name: &str) -> Result<()> {
        let Some(i) = self.project_index(name) else {
            bail!("unknown project {name:?}");
        };
        self.projects.remove(i);
        Ok(())
    }

    pub fn promote(&mut self, session: Key, stable: Key) {
        if self.contains(&stable) {
            self.unassign(&session);
        } else if let Some((i, j)) = self.position(&session) {
            let project = &mut self.projects[i];
            project.members[j] = stable.clone();
            if project.last == Some(session) {
                project.last = Some(stable);
            }
        }
    }

    pub fn observe_active(&mut self, key: Key) -> Option<ProjectPosition> {
        let (column, row) = self.position(&key)?;
        let project = &mut self.projects[column];
        project.last = Some(key);
        Some(ProjectPosition {
            name: project.name.clone(),
            row: row + 1,
            total: project.members.len(),
        })
    }

    pub fn go(
        &self,
        current: &Key,
        name: &str,
        available: impl Fn(&Key) -> bool,
    ) -> Result<Destination> {
        let Some(target) = self.project_index(name) else {
            bail!("unknown project {name:?}");
        };
        let side = if self
            .position(current)
            .is_some_and(|(from, _)| target < from)
        {
            Side::Left
        } else {
            Side::Right
        };
        self.projects[target]
            .destination(side, available)
            .ok_or_else(|| anyhow::anyhow!("no workspaces in {name:?} are available"))
    }

    pub fn neighbor(
        &self,
        current: &Key,
        direction: Direction,
        available: impl Fn(&Key) -> bool,
    ) -> Result<Option<Destination>> {
        let Some((column, row)) = self.position(current) else {
            bail!("active workspace is unassigned; use `cpw go PROJECT` or `cpw assign PROJECT`");
        };
        let members = &self.projects[column].members;
        Ok(match direction {
            Direction::Left if column == 0 => Some(Destination::BorrowSpare {
                project: SpareProject::Temporary,
                side: Side::Left,
            }),
            Direction::Right if column + 1 == self.projects.len() => {
                Some(Destination::BorrowSpare {
                    project: SpareProject::Temporary,
                    side: Side::Right,
                })
            }
            Direction::Left => self.projects[..column]
                .iter()
                .rev()
                .find_map(|project| project.destination(Side::Left, &available)),
            Direction::Right => self.projects[column + 1..]
                .iter()
                .find_map(|project| project.destination(Side::Right, &available)),
            Direction::Up => members[..row]
                .iter()
                .rev()
                .find(|key| available(key))
                .cloned()
                .map(Destination::Workspace),
            Direction::Down => members[row + 1..]
                .iter()
                .find(|key| available(key))
                .cloned()
                .map(Destination::Workspace),
        })
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name == name.trim()
            && name.chars().count() <= 64
            && !name.chars().any(char::is_control),
        "use a project name of 1–64 characters without surrounding spaces or control characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stable(id: &str) -> Key {
        Key::Stable(id.to_owned())
    }

    fn names(layout: &Layout) -> Vec<&str> {
        layout
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect()
    }

    #[test]
    fn new_layouts_have_named_starters_or_the_exact_custom_order() {
        let layout = Layout::initial("Work", Vec::new()).unwrap();
        assert_eq!(
            names(&Layout::restore(layout.snapshot()).unwrap()),
            ["Work", "Personal"]
        );
        assert_eq!(
            names(&Layout::initial("Personal", Vec::new()).unwrap()),
            ["Personal", "Work"]
        );
        let custom = Layout::initial("Work", vec!["Study".into(), "Work".into()]).unwrap();
        assert_eq!(names(&custom), ["Study", "Work"]);
        assert!(Layout::initial("Work", vec!["Study".into()]).is_err());
        assert!(Layout::initial("Work", vec!["Work".into(), "Work".into()]).is_err());
    }

    #[test]
    fn navigation_borrows_for_an_empty_neighbor_but_a_transfer_moves_the_existing_row() {
        let mut layout = Layout::initial("Work", Vec::new()).unwrap();
        let (work, hobby) = (stable("work"), stable("hobby"));
        layout.assign(work.clone(), "Work".into(), false).unwrap();
        layout.assign(hobby.clone(), "Hobby".into(), false).unwrap();
        assert_eq!(
            layout.neighbor(&work, Direction::Right, |_| true).unwrap(),
            Some(Destination::BorrowSpare {
                project: SpareProject::Existing("Personal".into()),
                side: Side::Right,
            })
        );
        assert_eq!(
            layout.neighbor(&hobby, Direction::Left, |_| true).unwrap(),
            Some(Destination::BorrowSpare {
                project: SpareProject::Existing("Personal".into()),
                side: Side::Left,
            })
        );
        assert_eq!(
            layout
                .move_to_neighbor(work.clone(), Side::Right, "Work")
                .unwrap(),
            "Personal"
        );
        assert_eq!(layout.project_name(&work), Some("Personal"));
        assert!(layout.projects[0].members.is_empty());
        assert!(
            layout
                .move_to_neighbor(Key::Session(99), Side::Right, "Work")
                .is_err()
        );
    }

    #[test]
    fn go_can_target_an_empty_project_from_either_side_or_the_unassigned_spare() {
        let mut layout = Layout::initial(
            "Work",
            vec!["Study".into(), "Work".into(), "Personal".into()],
        )
        .unwrap();
        let work = stable("work");
        layout.assign(work.clone(), "Work".into(), false).unwrap();
        for (current, project, side) in [
            (&work, "Study", Side::Left),
            (&work, "Personal", Side::Right),
            (&Key::Session(1), "Personal", Side::Right),
        ] {
            assert_eq!(
                layout.go(current, project, |_| true).unwrap(),
                Destination::BorrowSpare {
                    project: SpareProject::Existing(project.into()),
                    side
                }
            );
        }
        assert!(layout.go(&work, "Work", |_| false).is_err()); // Unavailable isn't empty.
        assert!(layout.go(&work, "Unknown", |_| true).is_err());
    }

    #[test]
    fn navigating_past_either_edge_borrows_without_transferring_the_current_workspace() {
        for (direction, side, expected) in [
            (Direction::Left, Side::Left, ["Project 1", "Work"]),
            (Direction::Right, Side::Right, ["Work", "Project 1"]),
        ] {
            let mut layout = Layout::initial("Work", vec!["Work".into()]).unwrap();
            let work = stable("work");
            let spare = Key::Session(2);
            layout.assign(work.clone(), "Work".into(), false).unwrap();
            assert_eq!(
                layout.neighbor(&work, direction, |_| true).unwrap(),
                Some(Destination::BorrowSpare {
                    project: SpareProject::Temporary,
                    side,
                })
            );
            assert_eq!(names(&layout), ["Work"]); // No column until a spare is assigned.
            let name = layout
                .assign_spare(spare.clone(), SpareProject::Temporary, side, "Work")
                .unwrap();
            assert_eq!(names(&layout), expected);
            assert!(layout.projects[layout.project_index(&name).unwrap()].temporary);
            assert_eq!(layout.project_name(&work), Some("Work"));
            assert_eq!(layout.project_name(&spare), Some(name.as_str()));

            layout.release_extension(&spare);
            assert_eq!(names(&layout), ["Work"]);
        }
    }

    #[test]
    fn reusing_an_empty_spare_prunes_its_preview_and_preserves_the_previous_real_row() {
        let mut layout = Layout::initial("Work", vec!["Work".into()]).unwrap();
        let (top, bottom, spare) = (stable("top"), stable("bottom"), Key::Session(3));
        layout.assign(top, "Work".into(), false).unwrap();
        layout.assign(bottom.clone(), "Work".into(), false).unwrap();
        layout.assign(spare.clone(), "Work".into(), false).unwrap();
        assert_eq!(
            layout
                .assign_spare(spare.clone(), SpareProject::Temporary, Side::Right, "Work")
                .unwrap(),
            "Project 1"
        );
        assert_eq!(
            layout.go(&spare, "Work", |_| true).unwrap(),
            Destination::Workspace(bottom.clone())
        );
        assert!(layout.is_single_row_temporary(&spare));
        assert!(!layout.is_single_row_temporary(&bottom));
        for name in ["Project 2", "Project 3", "Project 4"] {
            assert_eq!(
                layout
                    .assign_spare(spare.clone(), SpareProject::Temporary, Side::Right, "Work")
                    .unwrap(),
                name
            );
            assert_eq!(names(&layout), ["Work", name]);
            assert_eq!(layout.project_name(&spare), Some(name));
        }
        layout.release_extension(&spare);
        assert_eq!(
            layout.move_to_neighbor(bottom, Side::Left, "Work").unwrap(),
            "Project 1"
        );
        // Project 1 has a real workspace now, so a fresh preview can only recycle Project 2.
        assert_eq!(
            layout
                .assign_spare(spare, SpareProject::Temporary, Side::Left, "Work")
                .unwrap(),
            "Project 2"
        );
    }

    #[test]
    fn moving_past_an_edge_uses_an_unused_name_and_prunes_empty_temporary_columns() {
        let mut layout = Layout::initial("Work", vec!["Work".into(), "Project 1".into()]).unwrap();
        let key = stable("row");
        layout.assign(key.clone(), "Work".into(), false).unwrap();
        assert_eq!(
            layout
                .move_to_neighbor(key.clone(), Side::Left, "Project 2")
                .unwrap(),
            "Project 3"
        );
        assert_eq!(names(&layout), ["Project 3", "Work", "Project 1"]);
        assert!(layout.projects[0].temporary);
        let restored = Layout::restore(layout.snapshot()).unwrap();
        assert!(restored.projects[0].temporary);
        layout
            .move_to_neighbor(key.clone(), Side::Right, "Project 2")
            .unwrap();
        assert_eq!(names(&layout), ["Work", "Project 1"]);
        layout
            .move_to_neighbor(key.clone(), Side::Right, "Project 2")
            .unwrap();
        layout
            .move_to_neighbor(key.clone(), Side::Right, "Project 2")
            .unwrap();
        assert_eq!(names(&layout), ["Work", "Project 1", "Project 3"]);
        assert!(layout.unassign(&key));
        assert_eq!(names(&layout), ["Work", "Project 1"]);
    }

    #[test]
    fn renaming_keeps_a_column_when_empty_and_does_not_move_its_rows() {
        let mut layout = Layout::initial("Work", Vec::new()).unwrap();
        let key = stable("row");
        layout
            .assign(key.clone(), "Personal".into(), false)
            .unwrap();
        layout
            .move_to_neighbor(key.clone(), Side::Right, "Work")
            .unwrap();
        assert!(layout.rename_project("Project 1", "Work".into()).is_err());
        layout.rename_project("Project 1", "Study".into()).unwrap();
        assert_eq!(
            layout.go(&key, "Study", |_| true).unwrap(),
            Destination::Workspace(key.clone())
        );
        layout.move_to_neighbor(key, Side::Left, "Work").unwrap();
        let restored = Layout::restore(layout.snapshot()).unwrap();
        assert_eq!(names(&restored), ["Work", "Personal", "Study"]);
        assert!(restored.projects[2].members.is_empty());
    }

    #[test]
    fn legacy_saved_projects_are_named_and_unpinned_temporary_columns_are_not_saved() {
        let saved = serde_json::from_str(
            r#"{"projects":[{"name":"Work","members":["stable"],"last":"stable"}]}"#,
        )
        .unwrap();
        let mut layout = Layout::restore(saved).unwrap();
        let key = Key::Session(1);
        layout.assign(key.clone(), "Work".into(), false).unwrap();
        layout.move_to_neighbor(key, Side::Right, "Work").unwrap();
        assert_eq!(names(&layout), ["Work", "Project 1"]);
        assert_eq!(
            names(&Layout::restore(layout.snapshot()).unwrap()),
            ["Work"]
        );
    }

    #[test]
    fn default_assignment_preserves_explicit_projects_and_last_used() {
        let mut layout = Layout::default();
        let work = stable("work");
        let personal = stable("personal");
        let other = Key::Session(1);
        let another = Key::Session(2);
        layout.assign(work.clone(), "Work".into(), false).unwrap();
        layout
            .assign(personal.clone(), "Personal".into(), false)
            .unwrap();
        assert!(layout.assign_missing(
            "Work",
            vec![personal.clone(), other.clone(), another.clone()]
        ));
        assert_eq!(layout.projects[0].members, [work.clone(), other, another]);
        assert_eq!(layout.project_name(&personal), Some("Personal"));
        assert_eq!(
            layout.go(&personal, "Work", |_| true).unwrap(),
            Destination::Workspace(work.clone())
        );
        assert!(!layout.assign_missing("Other", vec![work, personal]));
        assert_eq!(layout.projects.len(), 2);
    }

    #[test]
    fn releasing_an_empty_extension_remembers_the_previous_row() {
        let mut layout = Layout::default();
        let bottom = stable("bottom");
        let spare = Key::Session(3);
        layout.assign(stable("top"), "Work".into(), false).unwrap();
        layout.assign(bottom.clone(), "Work".into(), false).unwrap();
        layout.assign(spare.clone(), "Work".into(), false).unwrap();
        layout.release_extension(&spare);
        assert_eq!(
            layout.go(&bottom, "Work", |_| true).unwrap(),
            Destination::Workspace(bottom.clone())
        );
    }

    #[test]
    fn horizontal_navigation_returns_to_last_used_not_same_row() {
        let mut l = Layout::default();
        let (a, b, c) = (stable("a"), stable("b"), stable("c"));
        l.assign(a.clone(), "Work".into(), false).unwrap();
        l.assign(b.clone(), "Personal".into(), false).unwrap();
        l.assign(c.clone(), "Personal".into(), false).unwrap();
        l.observe_active(b.clone()); // e.g. switching with COSMIC controls
        assert_eq!(
            l.neighbor(&a, Direction::Right, |_| true).unwrap(),
            Some(Destination::Workspace(b))
        );
        l.observe_active(c.clone());
        assert_eq!(
            l.neighbor(&a, Direction::Right, |_| true).unwrap(),
            Some(Destination::Workspace(c.clone()))
        );
        assert_eq!(
            l.neighbor(&c, Direction::Left, |_| true).unwrap(),
            Some(Destination::Workspace(a))
        );
    }

    #[test]
    fn vertical_navigation_respects_top_and_bottom_and_does_not_wrap() {
        let mut l = Layout::default();
        let (top, bottom) = (stable("a"), stable("b"));
        l.assign(bottom.clone(), "Work".into(), false).unwrap();
        l.assign(top.clone(), "Work".into(), true).unwrap();
        assert_eq!(
            l.neighbor(&top, Direction::Down, |_| true).unwrap(),
            Some(Destination::Workspace(bottom.clone()))
        );
        assert_eq!(
            l.neighbor(&bottom, Direction::Up, |_| true).unwrap(),
            Some(Destination::Workspace(top.clone()))
        );
        assert_eq!(l.neighbor(&top, Direction::Up, |_| true).unwrap(), None);
        assert_eq!(
            l.neighbor(&bottom, Direction::Down, |_| true).unwrap(),
            None
        );
    }

    #[test]
    fn skips_missing_columns_and_falls_back_to_top_available_row() {
        let mut l = Layout::default();
        let (a, b, c, d) = (stable("a"), stable("b"), stable("c"), stable("d"));
        l.assign(a.clone(), "Work".into(), false).unwrap();
        l.assign(b.clone(), "Hobby".into(), false).unwrap();
        l.assign(c.clone(), "Personal".into(), false).unwrap();
        l.assign(d.clone(), "Personal".into(), false).unwrap();
        let available = |key: &Key| *key != b && *key != d;
        assert_eq!(
            l.neighbor(&a, Direction::Right, available).unwrap(),
            Some(Destination::Workspace(c.clone()))
        );
        assert_eq!(l.neighbor(&c, Direction::Left, |_| false).unwrap(), None);
    }

    #[test]
    fn only_stable_assignments_are_saved_and_pinning_promotes_session_keys() {
        let mut l = Layout::default();
        let (a, b) = (stable("a"), stable("b"));
        l.assign(a.clone(), "Work".into(), false).unwrap();
        l.assign(Key::Session(4), "Work".into(), false).unwrap();
        l.assign(Key::Session(5), "Named".into(), false).unwrap();
        let restored = Layout::restore(l.snapshot()).unwrap();
        assert_eq!(names(&restored), ["Work", "Named"]);
        assert_eq!(restored.projects[0].members, std::slice::from_ref(&a));
        assert!(restored.projects[1].members.is_empty());
        l.promote(Key::Session(4), b.clone());
        let restored = Layout::restore(l.snapshot()).unwrap();
        assert_eq!(restored.projects[0].members, [a, b.clone()]);
        assert_eq!(restored.projects[0].last, Some(b));
    }

    #[test]
    fn moving_a_workspace_between_projects_does_not_duplicate_it() {
        let mut l = Layout::default();
        let a = stable("a");
        l.assign(a.clone(), "Work".into(), false).unwrap();
        l.assign(a.clone(), "Personal".into(), false).unwrap();
        assert_eq!(names(&l), ["Work", "Personal"]);
        assert_eq!(
            l.go(&a, "Work", |_| true).unwrap(),
            Destination::BorrowSpare {
                project: SpareProject::Existing("Work".into()),
                side: Side::Left,
            }
        );
        assert_eq!(
            l.go(&a, "Personal", |_| true).unwrap(),
            Destination::Workspace(a.clone())
        );
        assert!(l.unassign(&a));
        assert!(l.projects.iter().all(|p| p.members.is_empty()));
    }
}
