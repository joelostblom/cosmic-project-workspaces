mod occupancy;
mod overlay;

use crate::{
    Command,
    layout::{Destination, Direction, Key, Layout, SavedLayout, Side},
    order::OrderPlan,
    project_changes::ProjectChanges,
    slide::{ANIMATION_HOLD, CONFIG_SETTLE, SlideAxis, SlideExperiment},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use cosmic_protocols::workspace::v2::client::{
    zcosmic_workspace_handle_v2::{self as cosmic_workspace, ZcosmicWorkspaceHandleV2},
    zcosmic_workspace_manager_v2::ZcosmicWorkspaceManagerV2,
};
use occupancy::Occupancy;
use overlay::Overlay;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    io,
    os::unix::net::UnixListener,
    thread,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{ObjectId, WaylandError},
    event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry::WlRegistry,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self as group, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self as workspace, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self as manager, ExtWorkspaceManagerV1},
};

// COSMIC batches its workspace refreshes (150 ms as of 1.11). Give it time
// to append a new spare before releasing a just-left row as unused.
const LOAN_GRACE: Duration = Duration::from_millis(300);

pub struct Desktop {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    slide: Option<SlideExperiment>,
}

struct MoveSlide<'a> {
    active: &'a ObjectId,
    side: Side,
    source: &'a str,
    destination: &'a str,
}

impl Desktop {
    pub fn connect(
        layout: Layout,
        slide: Option<SlideExperiment>,
        default_project: String,
        enable_overlay: bool,
    ) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to the COSMIC Wayland session")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let manager = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("this session does not expose ext_workspace_manager_v1")?;
        let cosmic_manager = globals.bind(&queue.handle(), 2..=2, ()).ok();
        let overlay = enable_overlay
            .then(|| {
                Overlay::bind(&globals, &queue.handle())
                    .inspect_err(|error| eprintln!("Project overlay unavailable: {error:#}"))
                    .ok()
            })
            .flatten();
        let mut state = State {
            manager: Some(manager),
            cosmic_manager,
            layout,
            default_project,
            occupancy: Occupancy::bind(&globals, &queue.handle()),
            overlay,
            ..Default::default()
        };
        queue.roundtrip(&mut state)?;
        state.ensure_online()?;
        ensure!(
            state.ready,
            "the compositor did not send a workspace snapshot"
        );
        let mut desktop = Self {
            conn,
            queue,
            state,
            slide,
        };
        desktop.maintain_layout();
        Ok(desktop)
    }

    pub fn snapshot(&self) -> SavedLayout {
        self.state.layout.snapshot()
    }

    fn update_overlay(&mut self) -> Option<Duration> {
        let change = self.state.project_changes.take();
        let overlay = self.state.overlay.as_mut()?;
        overlay.expire(Instant::now());
        if let Some((change, position)) = change {
            overlay.update(change, position, &self.queue.handle());
        }
        overlay.time_remaining(Instant::now())
    }

    pub fn wait(&mut self, listener: &UnixListener) -> Result<bool> {
        self.maintain_layout();
        let overlay_timeout = self.update_overlay();
        // Wake even if neither connection receives events.
        let maintenance_timeout = (self.slide.is_some() || self.state.has_loan_deadline())
            .then_some(Duration::from_millis(100));
        let timeout = overlay_timeout.into_iter().chain(maintenance_timeout).min();
        self.poll_wayland(Some(listener), timeout)
    }

    fn hold_slide(&mut self) -> Result<()> {
        // Keep the axis stable for the compositor's animation, but continue
        // sending overlay requests and handling configure events immediately.
        let deadline = Instant::now() + ANIMATION_HOLD;
        loop {
            let overlay_timeout = self.update_overlay();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let timeout = overlay_timeout.map_or(remaining, |timeout| timeout.min(remaining));
            self.poll_wayland(None, Some(timeout))?;
        }
    }

    fn poll_wayland(
        &mut self,
        listener: Option<&UnixListener>,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        let Some(guard) = self.queue.prepare_read() else {
            self.queue.dispatch_pending(&mut self.state)?;
            self.state.ensure_online()?;
            return Ok(false);
        };

        let mut interest = PollFlags::IN;
        match self.conn.flush() {
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                interest |= PollFlags::OUT;
            }
            result => result?,
        }
        let mut fds = vec![PollFd::from_borrowed_fd(guard.connection_fd(), interest)];
        if let Some(listener) = listener {
            fds.push(PollFd::new(listener, PollFlags::IN));
        }
        let timeout = timeout.map(|duration| Timespec {
            tv_sec: duration.as_secs() as i64,
            tv_nsec: duration.subsec_nanos().into(),
        });
        match poll(&mut fds, timeout.as_ref()) {
            Err(rustix::io::Errno::INTR) => return Ok(false),
            result => {
                result?;
            }
        }
        let wayland = fds[0].revents();
        let command = fds.get(1).map_or(PollFlags::empty(), PollFd::revents);
        if wayland.intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) {
            match guard.read() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                result => {
                    result?;
                }
            }
        } else {
            drop(guard);
        }
        self.queue.dispatch_pending(&mut self.state)?;
        self.state.ensure_online()?;
        ensure!(
            !command.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL),
            "the command socket stopped listening"
        );
        Ok(command.contains(PollFlags::IN))
    }

    pub fn execute(&mut self, command: Command) -> Result<String> {
        // Read everything the compositor has sent before interpreting a shortcut.
        self.queue.roundtrip(&mut self.state)?;
        self.state.ensure_online()?;
        self.maintain_layout();
        match command {
            Command::Serve { .. } => bail!("the helper is already running"),
            Command::Status => Ok(self.state.status()),
            Command::SyncOrder { dry_run } => self.sync_order(dry_run),
            Command::Assign { project, top } => {
                let workspace = self.state.current()?;
                let (key, id, label) = (workspace.key(), workspace.handle.id(), workspace.label());
                self.state.layout.assign(key, project.clone(), top)?;
                Ok(self.finish_assignment(&id, label, &project, None))
            }
            Command::MoveLeft | Command::MoveRight => {
                let side = if matches!(command, Command::MoveLeft) {
                    Side::Left
                } else {
                    Side::Right
                };
                let workspace = self.state.current()?;
                let (key, id, label) = (workspace.key(), workspace.handle.id(), workspace.label());
                let source = self
                    .state
                    .layout
                    .project_name(&key)
                    .context("active workspace is unassigned; use `cpw assign PROJECT` first")?
                    .to_owned();
                let project =
                    self.state
                        .layout
                        .move_to_neighbor(key, side, &self.state.default_project)?;
                Ok(self.finish_assignment(&id, label, &project, Some((side, &source))))
            }
            Command::RenameProject { name } => {
                let key = self.state.current()?.key();
                let from = self
                    .state
                    .layout
                    .project_name(&key)
                    .context("active workspace is unassigned; use `cpw assign PROJECT` first")?
                    .to_owned();
                ensure!(
                    from != self.state.default_project || from == name,
                    "can't rename the default project for this run; choose a different default with `cpw serve --default-project NAME`"
                );
                self.state.layout.rename_project(&from, name.clone())?;
                self.state.observe_active();
                Ok(if from == name {
                    format!("Project {name} will be kept even when empty.")
                } else {
                    format!("Renamed project {from} to {name}.")
                })
            }
            Command::Unassign => {
                let workspace = self.state.current()?;
                let (key, name, id) = (workspace.key(), workspace.label(), workspace.handle.id());
                if self.state.layout.project_name(&key) == Some(self.state.default_project.as_str())
                    && self.state.default_candidates().contains(&key)
                {
                    return Ok(format!("{name} is already in the default project."));
                }
                ensure!(self.state.layout.unassign(&key), "{name} is not assigned");
                if let Some(workspace) = self.state.workspaces.get_mut(&id) {
                    workspace.extension = None;
                }
                self.maintain_layout();
                self.state.observe_active();
                Ok(match self.state.layout.project_name(&key) {
                    Some(project) => format!("Returned the active workspace to {project}."),
                    None => format!("Released {name} as the spare."),
                })
            }
            Command::Forget { project } => {
                ensure!(
                    project != self.state.default_project,
                    "can't forget the default project; select a different one with `serve --default-project NAME`"
                );
                self.state.layout.forget(&project)?;
                self.maintain_layout();
                self.state.observe_active();
                Ok(format!(
                    "Removed project {project}. Its available workspaces now use the default project ({}).",
                    self.state.default_project
                ))
            }
            Command::Go { project } => {
                let current = self.state.current()?.key();
                let target = self
                    .state
                    .layout
                    .go(&current, &project, |key| self.state.find(key).is_some())?;
                self.navigate(&current, target, SlideAxis::Horizontal)
            }
            Command::Left | Command::Right | Command::Up | Command::Down => {
                let direction = match command {
                    Command::Left => Direction::Left,
                    Command::Right => Direction::Right,
                    Command::Up => Direction::Up,
                    Command::Down => Direction::Down,
                    _ => unreachable!(),
                };
                let current = self.state.current()?.key();
                let target = self
                    .state
                    .layout
                    .neighbor(&current, direction, |key| self.state.find(key).is_some())?;
                match target {
                    Some(target) => self.navigate(
                        &current,
                        target,
                        match direction {
                            Direction::Left | Direction::Right => SlideAxis::Horizontal,
                            Direction::Up | Direction::Down => SlideAxis::Vertical,
                        },
                    ),
                    None if matches!(direction, Direction::Up | Direction::Down) => {
                        self.extend(&current, matches!(direction, Direction::Up))
                    }
                    None => Ok("No workspace in that direction.".into()),
                }
            }
        }
    }

    fn navigate(&mut self, current: &Key, target: Destination, axis: SlideAxis) -> Result<String> {
        match target {
            Destination::Workspace(key) => self.activate(key, axis, None),
            Destination::EmptyProject { name, side } => self.borrow_spare(
                current,
                name,
                SlideAxis::Horizontal,
                matches!(side, Side::Left),
            ),
        }
    }

    fn finish_assignment(
        &mut self,
        id: &ObjectId,
        label: String,
        project: &str,
        movement: Option<(Side, &str)>,
    ) -> String {
        if let Some(workspace) = self.state.workspaces.get_mut(id) {
            workspace.extension = None;
        }
        self.state.observe_active();
        if movement.is_some()
            && let Some(workspace) = self
                .state
                .workspaces
                .get(id)
                .filter(|workspace| workspace.active())
        {
            self.state.project_changes.announce_entry(workspace.serial);
        }
        let animation = movement.map(|(side, source)| MoveSlide {
            active: id,
            side,
            source,
            destination: project,
        });
        let warning = self
            .sync_order_with_slide(false, animation)
            .err()
            .map(|error| format!("\nCould not sync COSMIC order: {error:#}"))
            .unwrap_or_default();
        let workspace = self.state.workspaces.get(id);
        let label = workspace.map_or(label, Workspace::label);
        let hint = if workspace.is_some_and(|w| w.id.is_none()) {
            "\nSession only unless you pin this workspace in COSMIC."
        } else {
            ""
        };
        let verb = if movement.is_some() {
            "Moved"
        } else {
            "Assigned"
        };
        format!("{verb} {label} to {project}.{hint}{warning}")
    }

    fn extend(&mut self, current: &Key, top: bool) -> Result<String> {
        let origin = self.state.current()?;
        if origin.extension.as_ref().is_some_and(|extension| {
            matches!(
                extension.placement,
                ExtensionPlacement::Top | ExtensionPlacement::BorrowedLeft
            )
        }) && !self.state.occupancy.has_window(&origin.handle.id())
        {
            return Ok("Open a window on this new row before adding another.".into());
        }
        let project = self
            .state
            .layout
            .project_name(current)
            .context("the active workspace is not assigned to a project")?
            .to_owned();
        self.borrow_spare(current, project, SlideAxis::Vertical, top)
    }

    fn borrow_spare(
        &mut self,
        current: &Key,
        project: String,
        axis: SlideAxis,
        before: bool,
    ) -> Result<String> {
        ensure!(
            self.state.cosmic_manager.is_some(),
            "borrowing the spare requires COSMIC's dynamic workspaces"
        );
        if before && self.state.visible().any(|w| w.can_move.is_none()) {
            // Wait for the newest spare's reorder capabilities, if necessary.
            self.queue.roundtrip(&mut self.state)?;
            self.state.ensure_online()?;
        }
        let origin = self.state.current()?;
        ensure!(
            &origin.key() == current,
            "active workspace changed; try again"
        );
        let anchor = before.then(|| origin.handle.clone());
        let ordered = self.state.ordered_workspaces()?;
        let Some(spare) = ordered
            .last()
            .filter(|w| self.state.dynamic_empty(w) && !self.state.layout.contains(&w.key()))
        else {
            return Ok("No empty trailing workspace is available yet.".into());
        };
        let key = spare.key();
        let id = spare.handle.id();
        let serial = spare.serial;
        let already_active = spare.active();

        // Assign before activation so the confirmed switch sees the new project.
        // The spare is already after us for right/down; left/up needs an anchor.
        self.state
            .layout
            .assign(key.clone(), project.clone(), before)?;
        let placement = match (axis, before) {
            (SlideAxis::Vertical, true) => ExtensionPlacement::Top,
            (SlideAxis::Horizontal, true) => ExtensionPlacement::BorrowedLeft,
            (_, false) => ExtensionPlacement::BorrowedTail,
        };
        self.state.workspaces.get_mut(&id).unwrap().extension = Some(Extension {
            placement,
            inactive_since: None,
        });
        self.state.observe_active();
        if already_active {
            // `go PROJECT` can borrow the spare we are already sitting on.
            self.state.project_changes.announce_entry(serial);
        } else if let Err(error) = self.activate(key.clone(), axis, anchor.as_ref()) {
            let key = self.state.workspaces.get_mut(&id).map_or(key, |workspace| {
                workspace.extension = None;
                workspace.key()
            });
            self.state.layout.release_extension(&key);
            self.state.observe_active();
            return Err(error);
        }
        let workspace = self
            .state
            .workspaces
            .get(&id)
            .context("the new row is no longer available")?;
        let label = workspace.label();
        self.state.observe_active();
        match axis {
            SlideAxis::Horizontal => Ok(format!("Entered {project} on {label}.")),
            SlideAxis::Vertical => {
                let row = if before { "top row" } else { "row" };
                Ok(format!("Opened {label} as a new {row} in {project}."))
            }
        }
    }

    fn maintain_layout(&mut self) {
        let extended = self.state.refresh_extensions(Instant::now());
        // Window membership can change without a workspace Done event.
        if self.state.assign_defaults() {
            self.state.observe_active();
        }
        let assigned = std::mem::take(&mut self.state.assignment_order_changed);
        if (extended || assigned)
            && let Err(error) = self.sync_order(false)
        {
            eprintln!("Could not sync automatic workspace assignments: {error:#}");
        }
    }

    fn sync_order(&mut self, dry_run: bool) -> Result<String> {
        self.sync_order_with_slide(dry_run, None)
    }

    fn sync_order_with_slide(
        &mut self,
        dry_run: bool,
        animation: Option<MoveSlide<'_>>,
    ) -> Result<String> {
        // New handles may have been created during the previous roundtrip.
        if !dry_run
            && self.state.cosmic_manager.is_some()
            && self.state.visible().any(|w| w.can_move.is_none())
        {
            self.queue.roundtrip(&mut self.state)?;
            self.state.ensure_online()?;
        }
        let active = self.state.current()?;
        let current = self.state.ordered_workspaces()?;
        let ids: Vec<_> = current.iter().map(|w| w.serial).collect();
        let managed: Vec<_> = self
            .state
            .layout
            .projects
            .iter()
            .flat_map(|project| &project.members)
            .filter_map(|key| {
                let workspace = self.state.find(key)?;
                (!workspace.needs_alignment()).then_some(workspace.serial)
            })
            .collect();
        let plan = OrderPlan::new(&ids, &managed);
        let by_id: HashMap<_, _> = current.iter().map(|w| (w.serial, *w)).collect();
        if dry_run {
            if plan.moves.is_empty() {
                return Ok("COSMIC workspace order already matches.".into());
            }
            let describe = |order: &[u64]| {
                order
                    .iter()
                    .map(|id| {
                        let workspace = by_id[id];
                        let label = workspace.label();
                        match self.state.layout.project_name(&workspace.key()) {
                            Some(project) => format!("{label} [{project}]"),
                            None => label,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" → ")
            };
            return Ok(format!(
                "Current:  {}\nProposed: {}\nLabels are from the current COSMIC order.",
                describe(&ids),
                describe(&plan.expected)
            ));
        }
        let outgoing = self.slide.as_ref().and_then(|_| {
            let movement = animation.as_ref()?;
            if &active.handle.id() != movement.active || !active.can_activate {
                return None;
            }
            self.state.slide_reference(&plan, &by_id, active, movement)
        });
        if plan.moves.is_empty() && outgoing.is_none() {
            return Ok("COSMIC workspace order already matches.".into());
        }
        if !plan.moves.is_empty() {
            ensure!(
                self.state.cosmic_manager.is_some(),
                "this session doesn't expose COSMIC workspace reordering"
            );
        }
        // Check the entire batch before sending any requests. The target
        // anchor doesn't need to be movable, only the workspace being moved.
        let requests = plan
            .moves
            .iter()
            .map(|(id, anchor)| {
                let source = by_id[id];
                ensure!(
                    source.can_move == Some(true),
                    "{} can't be reordered",
                    source.label()
                );
                Ok((
                    source
                        .cosmic_handle
                        .as_ref()
                        .context("missing COSMIC workspace handle")?,
                    &by_id[anchor].handle,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let manager = self
            .state
            .manager
            .as_ref()
            .context("workspace manager went away")?;
        let (outgoing, override_layout) = match (&self.slide, outgoing) {
            (Some(slide), Some(outgoing)) => match slide.apply(SlideAxis::Horizontal) {
                Ok(guard) => (Some(outgoing), guard),
                Err(error) => {
                    eprintln!("Skipping the move animation: {error:#}");
                    (None, None)
                }
            },
            _ => (None, None),
        };
        if override_layout.is_some() {
            thread::sleep(CONFIG_SETTLE);
        }
        if requests.is_empty() && outgoing.is_none() {
            return Ok("COSMIC workspace order already matches.".into());
        }
        for (source, anchor) in requests {
            source.move_before(anchor, 0);
        }
        let animated_active = outgoing.map(|outgoing| {
            // Reordering the active handle alone never starts an animation.
            // In one commit, use a neighboring workspace as the outgoing
            // view, then switch back to the moved workspace in its new slot.
            outgoing.handle.activate();
            active.handle.activate();
            active.handle.id()
        });
        manager.commit();
        self.state.defer_active_observation = animated_active.is_some();
        let result = self.queue.roundtrip(&mut self.state);
        self.state.defer_active_observation = false;
        result?;
        if let Some(id) = animated_active {
            // The transient activation mustn't change project history or
            // emit a project-switch notification.
            self.state.observe_active();
            self.hold_slide()?;
            drop(override_layout);
            ensure!(
                self.state
                    .workspaces
                    .get(&id)
                    .is_some_and(Workspace::active),
                "COSMIC didn't confirm that the moved workspace stayed active"
            );
        }
        self.state.ensure_online()?;
        ensure!(
            self.state
                .ordered_workspaces()?
                .into_iter()
                .map(|w| w.serial)
                .eq(plan.expected),
            "COSMIC didn't confirm the planned order; check `cpw sync-order --dry-run`"
        );
        Ok("Synced COSMIC workspace order.".into())
    }

    fn activate(
        &mut self,
        key: Key,
        axis: SlideAxis,
        move_before: Option<&ExtWorkspaceHandleV1>,
    ) -> Result<String> {
        let workspace = self
            .state
            .find(&key)
            .context("the destination workspace is no longer available")?;
        let name = workspace.label();
        if workspace.active() {
            return Ok(format!("Already on {name}."));
        }
        ensure!(
            workspace.can_activate,
            "COSMIC won't let the helper activate {name}"
        );
        let reposition = if let Some(anchor) = move_before {
            ensure!(
                workspace.can_move == Some(true),
                "COSMIC won't let the helper move {name}"
            );
            let handle = workspace
                .cosmic_handle
                .as_ref()
                .context("missing COSMIC workspace handle")?;
            Some((handle, anchor))
        } else {
            None
        };
        let manager = self
            .state
            .manager
            .as_ref()
            .context("workspace manager went away")?;
        let (wait_for_slide, override_layout) = match &self.slide {
            Some(slide) => match slide.apply(axis) {
                Ok(guard) => (true, guard),
                Err(error) => {
                    eprintln!("Using COSMIC's native slide for this switch: {error:#}");
                    (false, None)
                }
            },
            None => (false, None),
        };
        if override_layout.is_some() {
            thread::sleep(CONFIG_SETTLE);
        }
        if let Some((handle, anchor)) = reposition {
            // Commit both requests together. COSMIC picks the animation
            // direction from the newly ordered indices, without refreshing
            // (and potentially dropping the empty workspace) in between.
            handle.move_before(anchor, 0);
        }
        workspace.handle.activate();
        manager.commit();
        self.queue.roundtrip(&mut self.state)?;
        // Serialize experimental moves even when the requested axis already
        // matches. The next shortcut must not flip it during this animation.
        if wait_for_slide {
            self.hold_slide()?;
        }
        drop(override_layout);
        let current = self
            .state
            .find(&key)
            .filter(|w| w.active())
            .with_context(|| format!("COSMIC did not confirm activation of {name}"))?;
        if let Some(anchor) = move_before.and_then(|handle| self.state.workspaces.get(&handle.id()))
        {
            ensure!(
                current
                    .coordinate
                    .zip(anchor.coordinate)
                    .is_some_and(|(to, from)| to < from),
                "COSMIC did not move {name} above the previous workspace"
            );
        }
        Ok(format!("Switched to {}.", current.label()))
    }
}

#[derive(Default)]
struct State {
    manager: Option<ExtWorkspaceManagerV1>,
    cosmic_manager: Option<ZcosmicWorkspaceManagerV2>,
    ready: bool,
    groups: HashSet<ObjectId>,
    workspaces: HashMap<ObjectId, Workspace>,
    next_serial: u64,
    layout: Layout,
    default_project: String,
    assignment_order_changed: bool,
    project_changes: ProjectChanges,
    defer_active_observation: bool,
    occupancy: Occupancy,
    overlay: Option<Overlay>,
}

struct Workspace {
    handle: ExtWorkspaceHandleV1,
    cosmic_handle: Option<ZcosmicWorkspaceHandleV2>,
    group: Option<ObjectId>,
    serial: u64,
    id: Option<String>,
    name: Option<String>,
    coordinate: Option<u32>,
    state: workspace::State,
    can_activate: bool,
    can_move: Option<bool>,
    pinned: bool,
    extension: Option<Extension>,
}

struct Extension {
    placement: ExtensionPlacement,
    inactive_since: Option<Instant>,
}

#[derive(Clone, Copy)]
enum ExtensionPlacement {
    Top,
    BorrowedTail,
    BorrowedLeft,
    Aligned,
}

impl Workspace {
    fn key(&self) -> Key {
        match &self.id {
            Some(id) => Key::Stable(id.clone()),
            None => Key::Session(self.serial),
        }
    }

    fn active(&self) -> bool {
        self.state.contains(workspace::State::Active)
    }

    fn label(&self) -> String {
        format!("workspace {}", self.name.as_deref().unwrap_or("(unnamed)"))
    }

    fn needs_alignment(&self) -> bool {
        self.extension.as_ref().is_some_and(|loan| {
            matches!(
                loan.placement,
                ExtensionPlacement::BorrowedTail | ExtensionPlacement::BorrowedLeft
            )
        })
    }
}

impl State {
    fn ensure_online(&self) -> Result<()> {
        ensure!(
            self.manager.is_some(),
            "the compositor stopped reporting workspaces"
        );
        Ok(())
    }

    fn visible(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.values().filter(|w| {
            w.group
                .as_ref()
                .is_some_and(|group| self.groups.contains(group))
                && !w.state.contains(workspace::State::Hidden)
        })
    }

    fn find(&self, key: &Key) -> Option<&Workspace> {
        self.visible().find(|w| match key {
            Key::Stable(id) => w.id.as_ref() == Some(id),
            Key::Session(serial) => w.id.is_none() && w.serial == *serial,
        })
    }

    fn ordered_workspaces(&self) -> Result<Vec<&Workspace>> {
        let mut workspaces: Vec<_> = self.visible().collect();
        workspaces.sort_by_key(|w| w.coordinate);
        ensure!(
            workspaces.iter().all(|w| w.coordinate.is_some())
                && workspaces
                    .windows(2)
                    .all(|pair| pair[0].coordinate != pair[1].coordinate),
            "COSMIC didn't provide a unique linear position for each workspace"
        );
        Ok(workspaces)
    }

    fn dynamic_empty(&self, workspace: &Workspace) -> bool {
        workspace.id.is_none()
            && !workspace.pinned
            && !self.occupancy.has_window(&workspace.handle.id())
    }

    fn slide_reference<'a>(
        &self,
        plan: &OrderPlan,
        workspaces_by_serial: &HashMap<u64, &'a Workspace>,
        active: &Workspace,
        movement: &MoveSlide<'_>,
    ) -> Option<&'a Workspace> {
        let occupied_or_pinned =
            |w: &Workspace| w.pinned || self.occupancy.has_window(&w.handle.id());
        let usable = |id| {
            let w = workspaces_by_serial[&id];
            // COSMIC clears workspace urgency on activation.
            w.can_activate && !w.state.contains(workspace::State::Urgent)
        };
        let reference = plan
            .outgoing_workspace(active.serial, movement.side, |id| {
                let w = workspaces_by_serial[&id];
                let project = self.layout.project_name(&w.key());
                usable(id)
                    && occupied_or_pinned(w)
                    && (project == Some(movement.source) || project == Some(movement.destination))
            })
            .or_else(|| {
                plan.outgoing_workspace(active.serial, movement.side, |id| {
                    let w = workspaces_by_serial[&id];
                    usable(id)
                        && plan.expected.last() == Some(&id)
                        && self.dynamic_empty(w)
                        && !self.layout.contains(&w.key())
                        && occupied_or_pinned(active)
                })
            })
            .or_else(|| {
                plan.outgoing_workspace(active.serial, movement.side, |id| {
                    usable(id) && occupied_or_pinned(workspaces_by_serial[&id])
                })
            })?;
        workspaces_by_serial.get(&reference).copied()
    }

    fn default_candidates(&self) -> Vec<Key> {
        if self.groups.len() != 1 {
            return Vec::new();
        }
        let Ok(ordered) = self.ordered_workspaces() else {
            return Vec::new();
        };
        let Some((tail, preceding)) = ordered.split_last() else {
            return Vec::new();
        };
        // Leave COSMIC's one empty trailing workspace free to grow any column.
        preceding
            .iter()
            .copied()
            .chain((!self.dynamic_empty(tail)).then_some(*tail))
            .map(Workspace::key)
            .collect()
    }

    fn assign_defaults(&mut self) -> bool {
        let candidates = self.default_candidates();
        let assigned = self
            .layout
            .assign_missing(&self.default_project, candidates);
        self.assignment_order_changed |= assigned;
        assigned
    }

    fn has_loan_deadline(&self) -> bool {
        self.workspaces.values().any(|w| {
            w.extension
                .as_ref()
                .is_some_and(|row| row.inactive_since.is_some())
        })
    }

    fn refresh_extensions(&mut self, now: Instant) -> bool {
        if self.groups.len() != 1 || !self.workspaces.values().any(|w| w.extension.is_some()) {
            return false;
        }
        let Ok(ordered) = self.ordered_workspaces() else {
            return false;
        };
        let Some(tail) = ordered.last() else {
            return false;
        };
        let (tail_serial, tail_position) = (tail.serial, tail.coordinate);
        let mut sync = false;
        let mut released = false;
        for workspace in self.workspaces.values_mut() {
            let key = workspace.key();
            let active = workspace.active();
            let Some(extension) = workspace.extension.as_mut() else {
                continue;
            };
            let occupied = self.occupancy.has_window(&workspace.handle.id());
            if !self.layout.contains(&key) {
                workspace.extension = None;
            } else if workspace.serial == tail_serial && !active && !occupied && !workspace.pinned {
                // COSMIC keeps the last empty workspace even after you leave
                // it. Drop our assignment so another project can borrow it.
                let since = extension.inactive_since.get_or_insert(now);
                if now.duration_since(*since) >= LOAN_GRACE {
                    self.layout.release_extension(&key);
                    workspace.extension = None;
                    released = true;
                }
            } else {
                extension.inactive_since = None;
                let ready = match extension.placement {
                    // The tail loan can join the block once COSMIC has made a new spare.
                    ExtensionPlacement::BorrowedTail => {
                        tail_serial > workspace.serial && workspace.coordinate < tail_position
                    }
                    // A leftward loan was placed before the outgoing workspace for the
                    // slide, not necessarily at the start of its project block.
                    ExtensionPlacement::BorrowedLeft => occupied,
                    _ => false,
                };
                if ready {
                    extension.placement = ExtensionPlacement::Aligned;
                    sync = true;
                }
            }
        }
        if released {
            self.observe_active();
        }
        sync
    }

    fn current(&self) -> Result<&Workspace> {
        ensure!(
            self.groups.len() == 1,
            "the prototype currently requires a single active display"
        );
        let mut active = self.visible().filter(|w| w.active());
        let current = active
            .next()
            .ok_or_else(|| anyhow!("no active workspace found"))?;
        ensure!(active.next().is_none(), "more than one workspace is active");
        Ok(current)
    }

    fn observe_active(&mut self) {
        if self.defer_active_observation {
            return;
        }
        let current = self.current().ok().map(|w| (w.serial, w.key()));
        let active = current.map(|(serial, key)| (serial, self.layout.observe_active(key)));
        self.project_changes.observe(active);
    }

    fn status(&self) -> String {
        let mut text = String::new();
        if let Err(error) = self.current() {
            writeln!(text, "Navigation unavailable: {error}").unwrap();
        }
        if self.layout.projects.is_empty() {
            writeln!(
                text,
                "Open a window to start the default project, or run `cpw assign PROJECT`."
            )
            .unwrap();
        } else {
            self.write_grid(&mut text);
        }
        let spare = self
            .ordered_workspaces()
            .ok()
            .and_then(|workspaces| workspaces.last().copied())
            .filter(|w| {
                self.groups.len() == 1 && self.dynamic_empty(w) && !self.layout.contains(&w.key())
            });
        let mut unassigned: Vec<_> = self
            .visible()
            .filter(|w| {
                !self.layout.contains(&w.key())
                    && spare.is_none_or(|spare| spare.serial != w.serial)
            })
            .collect();
        unassigned.sort_by_key(|w| (w.coordinate, w.serial));
        if !unassigned.is_empty() {
            writeln!(text, "\nUnassigned:").unwrap();
            for w in unassigned {
                writeln!(text, "   {}", Self::status_label(w)).unwrap();
            }
        }
        writeln!(text, "\n[ ] active; * last used").unwrap();
        writeln!(text, "Default project: {}", self.default_project).unwrap();
        let spare = spare.map_or_else(|| "none".to_owned(), Self::status_label);
        write!(text, "Spare: {spare}").unwrap();
        text
    }

    fn status_label(workspace: &Workspace) -> String {
        let label = workspace.label();
        if workspace.active() {
            format!("[{label}]")
        } else {
            label
        }
    }

    fn write_grid(&self, text: &mut String) {
        let columns: Vec<_> = self
            .layout
            .projects
            .iter()
            .map(|project| {
                let heading = if project.temporary {
                    format!("{} (temporary)", project.name)
                } else {
                    project.name.clone()
                };
                let mut cells: Vec<_> = std::iter::once(heading)
                    .chain(project.members.iter().map(|key| {
                        let mut label = self
                            .find(key)
                            .map_or_else(|| "(unavailable)".into(), Self::status_label);
                        if project.last.as_ref() == Some(key) {
                            label.push_str(" *");
                        }
                        label
                    }))
                    .collect();
                if project.members.is_empty() {
                    cells.push("(empty)".into());
                }
                cells
            })
            .collect();
        write_status_columns(text, &columns);
    }
}

fn write_status_columns(text: &mut String, columns: &[Vec<String>]) {
    const SEPARATOR: &str = " │ ";
    let widths: Vec<_> = columns
        .iter()
        .map(|cells| cells.iter().map(|cell| cell.width()).max().unwrap_or(0))
        .collect();
    let height = columns.iter().map(Vec::len).max().unwrap_or(0);
    for row in 0..height {
        let mut line = String::new();
        for (index, (cells, width)) in columns.iter().zip(&widths).enumerate() {
            if index > 0 {
                line.push_str(SEPARATOR);
            }
            let cell = cells.get(row).map_or("", String::as_str);
            line.push_str(cell);
            line.extend(std::iter::repeat_n(' ', width - cell.width()));
        }
        writeln!(text, "{}", line.trim_end()).unwrap();
        if row == 0 {
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    text.push_str("-|-");
                }
                text.extend(std::iter::repeat_n('-', *width));
            }
            text.push('\n');
        }
    }
}

#[cfg(test)]
#[test]
fn status_grid_separates_headings_and_aligns_wide_names() {
    let columns = [
        vec!["仕事".into(), "[1] *".into(), "2".into()],
        vec!["Personal".into(), "(empty)".into()],
    ];
    let mut grid = String::new();
    write_status_columns(&mut grid, &columns);
    assert_eq!(
        grid,
        "仕事  │ Personal\n------|---------\n[1] * │ (empty)\n2     │\n"
    );
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    event_created_child!(State, ExtWorkspaceManagerV1, [
        manager::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        manager::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: manager::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            manager::Event::WorkspaceGroup { workspace_group } => {
                state.groups.insert(workspace_group.id());
            }
            manager::Event::Workspace { workspace } => {
                state.next_serial += 1;
                let id = workspace.id();
                let cosmic_handle = state
                    .cosmic_manager
                    .as_ref()
                    .map(|manager| manager.get_cosmic_workspace(&workspace, qh, id.clone()));
                state.workspaces.insert(
                    id,
                    Workspace {
                        handle: workspace,
                        cosmic_handle,
                        group: None,
                        serial: state.next_serial,
                        id: None,
                        name: None,
                        coordinate: None,
                        state: workspace::State::empty(),
                        can_activate: false,
                        can_move: None,
                        pinned: false,
                        extension: None,
                    },
                );
            }
            manager::Event::Done => {
                state.ready = true;
                state.assign_defaults();
                state.observe_active();
            }
            manager::Event::Finished => state.manager = None,
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: group::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            group::Event::WorkspaceEnter { workspace } => {
                if let Some(w) = state.workspaces.get_mut(&workspace.id()) {
                    w.group = Some(group.id());
                }
            }
            group::Event::WorkspaceLeave { workspace } => {
                if let Some(w) = state.workspaces.get_mut(&workspace.id())
                    && w.group.as_ref() == Some(&group.id())
                {
                    w.group = None;
                }
            }
            group::Event::Removed => {
                state.groups.remove(&group.id());
                group.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: workspace::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(w) = state.workspaces.get_mut(&handle.id()) else {
            return;
        };
        match event {
            workspace::Event::Id { id } => {
                if !id.is_empty() {
                    state.assignment_order_changed |= w.needs_alignment();
                    state.layout.promote(w.key(), Key::Stable(id.clone()));
                    w.id = Some(id);
                    w.extension = None;
                }
            }
            workspace::Event::Name { name } => w.name = Some(name),
            workspace::Event::Coordinates { coordinates } => {
                w.coordinate = coordinates
                    .as_slice()
                    .try_into()
                    .ok()
                    .map(u32::from_ne_bytes);
            }
            workspace::Event::State { state } => {
                w.state = workspace::State::from_bits_truncate(state.into());
            }
            workspace::Event::Capabilities { capabilities } => {
                w.can_activate =
                    workspace::WorkspaceCapabilities::from_bits_truncate(capabilities.into())
                        .contains(workspace::WorkspaceCapabilities::Activate);
            }
            workspace::Event::Removed => {
                if w.extension.is_some() {
                    state.layout.release_extension(&w.key());
                } else if w.id.is_none() {
                    state.layout.unassign(&w.key());
                }
                if let Some(cosmic_handle) = w.cosmic_handle.take() {
                    cosmic_handle.destroy();
                }
                state.workspaces.remove(&handle.id());
                handle.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicWorkspaceManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicWorkspaceManagerV2,
        _: <ZcosmicWorkspaceManagerV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZcosmicWorkspaceHandleV2, ObjectId> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicWorkspaceHandleV2,
        event: cosmic_workspace::Event,
        id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(workspace) = state.workspaces.get_mut(id) else {
            return;
        };
        match event {
            cosmic_workspace::Event::Capabilities { capabilities } => {
                workspace.can_move = Some(
                    cosmic_workspace::WorkspaceCapabilities::from_bits_truncate(
                        capabilities.into(),
                    )
                    .contains(cosmic_workspace::WorkspaceCapabilities::Move),
                );
            }
            cosmic_workspace::Event::State { state } => {
                workspace.pinned = cosmic_workspace::State::from_bits_truncate(state.into())
                    .contains(cosmic_workspace::State::Pinned);
            }
            _ => {}
        }
    }
}
