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

#[derive(Default)]
pub struct Layout {
    pub projects: Vec<Project>,
}

pub struct Project {
    pub name: String,
    pub members: Vec<Key>,
    pub last: Option<Key>,
}

pub struct ProjectPosition {
    pub name: String,
    pub row: usize,
    pub total: usize,
}

impl ProjectPosition {
    pub fn title(&self) -> String {
        format!("{} ({}/{})", self.name, self.row, self.total)
    }
}

impl Project {
    pub fn entry(&self, available: impl Fn(&Key) -> bool) -> Option<&Key> {
        self.last
            .as_ref()
            .filter(|key| available(key))
            .or_else(|| self.members.iter().find(|key| available(key)))
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
}

impl Layout {
    pub fn restore(saved: SavedLayout) -> Result<Self> {
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        for project in &saved.projects {
            validate_name(&project.name)?;
            ensure!(names.insert(&project.name), "duplicate project name");
            ensure!(
                !project.members.is_empty() && project.members.iter().all(|id| ids.insert(id)),
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
                .map(|project| Project {
                    name: project.name,
                    members: project.members.into_iter().map(Key::Stable).collect(),
                    last: project.last.map(Key::Stable),
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
                    (!members.is_empty()).then(|| SavedProject {
                        name: project.name.clone(),
                        members,
                        last: project
                            .last
                            .as_ref()
                            .and_then(Key::stable)
                            .map(str::to_owned),
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

    pub fn assign(&mut self, key: Key, name: String, top: bool) -> Result<()> {
        validate_name(&name)?;
        self.detach(&key);
        let index = self
            .projects
            .iter()
            .position(|project| project.name == name);
        let project = if let Some(index) = index {
            &mut self.projects[index]
        } else {
            self.projects.push(Project {
                name,
                members: Vec::new(),
                last: None,
            });
            self.projects.last_mut().unwrap()
        };
        let row = if top { 0 } else { project.members.len() };
        project.members.insert(row, key.clone());
        project.last = Some(key);
        self.drop_empty();
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
        self.projects.retain(|project| !project.members.is_empty());
    }

    pub fn unassign(&mut self, key: &Key) -> bool {
        let detached = self.detach(key);
        self.drop_empty();
        detached
    }

    pub fn forget(&mut self, name: &str) -> Result<()> {
        let Some(i) = self
            .projects
            .iter()
            .position(|project| project.name == name)
        else {
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

    pub fn go(&self, name: &str, available: impl Fn(&Key) -> bool) -> Result<&Key> {
        let Some(project) = self.projects.iter().find(|p| p.name == name) else {
            bail!("unknown project {name:?}");
        };
        project
            .entry(available)
            .ok_or_else(|| anyhow::anyhow!("no workspaces in {name:?} are available"))
    }

    pub fn neighbor(
        &self,
        current: &Key,
        direction: Direction,
        available: impl Fn(&Key) -> bool,
    ) -> Result<Option<&Key>> {
        let Some((column, row)) = self.position(current) else {
            bail!("active workspace is unassigned; use `cpw go PROJECT` or `cpw assign PROJECT`");
        };
        let members = &self.projects[column].members;
        Ok(match direction {
            Direction::Left => self.projects[..column]
                .iter()
                .rev()
                .find_map(|project| project.entry(&available)),
            Direction::Right => self.projects[column + 1..]
                .iter()
                .find_map(|project| project.entry(&available)),
            Direction::Up => members[..row].iter().rev().find(|key| available(key)),
            Direction::Down => members[row + 1..].iter().find(|key| available(key)),
        })
    }
}

fn validate_name(name: &str) -> Result<()> {
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
            Some(&b)
        );
        l.observe_active(c.clone());
        assert_eq!(
            l.neighbor(&a, Direction::Right, |_| true).unwrap(),
            Some(&c)
        );
        assert_eq!(l.neighbor(&c, Direction::Left, |_| true).unwrap(), Some(&a));
    }

    #[test]
    fn vertical_navigation_respects_top_and_bottom_and_does_not_wrap() {
        let mut l = Layout::default();
        let (top, bottom) = (stable("a"), stable("b"));
        l.assign(bottom.clone(), "Work".into(), false).unwrap();
        l.assign(top.clone(), "Work".into(), true).unwrap();
        assert_eq!(
            l.neighbor(&top, Direction::Down, |_| true).unwrap(),
            Some(&bottom)
        );
        assert_eq!(
            l.neighbor(&bottom, Direction::Up, |_| true).unwrap(),
            Some(&top)
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
            Some(&c)
        );
    }

    #[test]
    fn only_stable_assignments_are_saved_and_pinning_promotes_session_keys() {
        let mut l = Layout::default();
        let (a, b) = (stable("a"), stable("b"));
        l.assign(a.clone(), "Work".into(), false).unwrap();
        l.assign(Key::Session(4), "Work".into(), false).unwrap();
        l.assign(Key::Session(5), "Temporary".into(), false)
            .unwrap();
        let restored = Layout::restore(l.snapshot()).unwrap();
        assert_eq!(restored.projects.len(), 1);
        assert_eq!(restored.projects[0].members, std::slice::from_ref(&a));
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
        assert_eq!(l.projects.len(), 1);
        assert!(l.go("Work", |_| true).is_err());
        assert_eq!(l.go("Personal", |_| true).unwrap(), &a);
        assert!(l.unassign(&a));
        assert!(l.projects.is_empty());
    }
}
