use anyhow::{Context, Result, ensure};
use std::{
    io,
    process::{Command, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct ProjectChanges {
    active: Option<(u64, Option<String>)>,
    pending: Option<String>,
}

impl ProjectChanges {
    pub fn observe(&mut self, active: Option<(u64, Option<String>)>) {
        match (&self.active, &active) {
            (Some((previous, before)), Some((current, after))) if before != after => {
                // Edits or pinning can change the project mapping without changing desktops.
                self.pending = if previous != current {
                    after.clone()
                } else {
                    None
                };
            }
            (_, None) => self.pending = None,
            _ => {}
        }
        self.active = active;
    }

    pub fn take(&mut self) -> Option<String> {
        self.pending.take()
    }
}

pub struct Notifier(Sender<String>);

impl Notifier {
    pub fn start() -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("project-notifications".into())
            .spawn(move || {
                let mut previous: Option<(u32, Instant)> = None;
                let mut reported_error = false;
                while let Ok(project) = rx.recv() {
                    // Keep switching responsive if the notification server is slow.
                    let project = rx.try_iter().last().unwrap_or(project);
                    let replacement =
                        previous.filter(|(_, started)| started.elapsed() < Duration::from_secs(1));
                    let now = Instant::now();
                    match notify(&project, replacement.map(|(id, _)| id)) {
                        Ok(id) => {
                            let started = replacement.map_or(now, |(_, started)| started);
                            previous = id.map(|id| (id, started));
                            reported_error = false;
                        }
                        Err(error) => {
                            previous = None;
                            if !reported_error {
                                eprintln!("Project notifications unavailable: {error:#}");
                                reported_error = true;
                            }
                        }
                    }
                }
            })?;
        Ok(Self(tx))
    }

    pub fn show(&self, project: String) {
        let _ = self.0.send(project);
    }
}

fn notify(project: &str, replace: Option<u32>) -> Result<Option<u32>> {
    let mut cmd = Command::new("notify-send");
    cmd.args([
        "--app-name=Project Workspaces",
        "--urgency=low",
        "--transient",
        "--expire-time=1800",
        "--print-id",
    ]);
    if let Some(id) = replace {
        cmd.arg(format!("--replace-id={id}"));
    }
    let result = cmd
        .args(["--", project])
        .stdin(Stdio::null())
        .output()
        .context("run notify-send")?;
    ensure!(
        result.status.success(),
        "notify-send failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&result.stdout)
        .trim()
        .parse()
        .ok()
        .filter(|&id| id != 0))
}

#[cfg(test)]
mod tests {
    use super::ProjectChanges;

    #[test]
    fn announces_only_entry_to_a_different_assigned_project() {
        let mut changes = ProjectChanges::default();
        changes.observe(Some((1, Some("Work".into()))));
        assert_eq!(changes.take(), None); // Initial snapshot.
        changes.observe(Some((2, Some("Work".into()))));
        assert_eq!(changes.take(), None); // A different row.
        changes.observe(Some((3, Some("Personal".into()))));
        assert_eq!(changes.take().as_deref(), Some("Personal"));
        changes.observe(Some((3, Some("Hobby".into()))));
        assert_eq!(changes.take(), None); // An assignment edit.
        changes.observe(Some((4, None)));
        assert_eq!(changes.take(), None);
        changes.observe(Some((1, Some("Work".into()))));
        assert_eq!(changes.take().as_deref(), Some("Work"));
    }

    #[test]
    fn coalesces_switches_without_leaving_a_stale_banner() {
        let mut changes = ProjectChanges::default();
        changes.observe(Some((1, Some("Work".into()))));
        changes.observe(Some((2, Some("Personal".into()))));
        changes.observe(Some((3, Some("Hobby".into()))));
        assert_eq!(changes.take().as_deref(), Some("Hobby"));
        changes.observe(Some((2, Some("Personal".into()))));
        changes.observe(Some((4, None)));
        assert_eq!(changes.take(), None);
    }
}
