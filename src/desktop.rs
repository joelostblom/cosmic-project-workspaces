use crate::{
    Command,
    layout::{Direction, Key, Layout, SavedLayout},
    notifications::ProjectChanges,
    order::OrderPlan,
    slide::{ANIMATION_HOLD, CONFIG_SETTLE, SlideAxis, SlideExperiment},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use cosmic_protocols::workspace::v2::client::{
    zcosmic_workspace_handle_v2::{self as cosmic_workspace, ZcosmicWorkspaceHandleV2},
    zcosmic_workspace_manager_v2::ZcosmicWorkspaceManagerV2,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    io,
    os::unix::net::UnixListener,
    thread,
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

pub struct Desktop {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    slide: Option<SlideExperiment>,
}

impl Desktop {
    pub fn connect(layout: Layout, slide: Option<SlideExperiment>) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to the COSMIC Wayland session")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let manager = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("this session does not expose ext_workspace_manager_v1")?;
        let cosmic_manager = globals.bind(&queue.handle(), 2..=2, ()).ok();
        let mut state = State {
            manager: Some(manager),
            cosmic_manager,
            layout,
            ..Default::default()
        };
        queue.roundtrip(&mut state)?;
        state.ensure_online()?;
        ensure!(
            state.ready,
            "the compositor did not send a workspace snapshot"
        );
        Ok(Self {
            conn,
            queue,
            state,
            slide,
        })
    }

    pub fn snapshot(&self) -> SavedLayout {
        self.state.layout.snapshot()
    }

    pub fn take_project_notification(&mut self) -> Option<String> {
        self.state.project_changes.take()
    }

    pub fn wait(&mut self, listener: &UnixListener) -> Result<bool> {
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
        let mut fds = [
            PollFd::from_borrowed_fd(guard.connection_fd(), interest),
            PollFd::new(listener, PollFlags::IN),
        ];
        // When the experiment catches termination signals for cleanup, check the
        // stop flag periodically even if neither connection receives events.
        let timeout = self.slide.as_ref().map(|_| Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        });
        match poll(&mut fds, timeout.as_ref()) {
            Err(rustix::io::Errno::INTR) => return Ok(false),
            result => {
                result?;
            }
        }
        let wayland = fds[0].revents();
        let command = fds[1].revents();
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
        match command {
            Command::Serve { .. } => bail!("the helper is already running"),
            Command::Status { grid } => Ok(self.state.status(grid)),
            Command::SyncOrder { dry_run } => self.sync_order(dry_run),
            Command::Assign { project, top } => {
                let workspace = self.state.current()?;
                let (key, serial, old_name) =
                    (workspace.key(), workspace.serial, workspace.label());
                let hint = if matches!(key, Key::Session(_)) {
                    "\nSession only unless you pin this workspace in COSMIC."
                } else {
                    ""
                };
                self.state.layout.assign(key, project.clone(), top)?;
                self.state.observe_active();
                let warning = self
                    .sync_order(false)
                    .err()
                    .map(|error| format!("\nCould not sync COSMIC order: {error:#}"))
                    .unwrap_or_default();
                let name = self
                    .state
                    .workspaces
                    .values()
                    .find(|w| w.serial == serial)
                    .map_or(old_name, Workspace::label);
                Ok(format!("Assigned {name} to {project}.{hint}{warning}"))
            }
            Command::Unassign => {
                let workspace = self.state.current()?;
                let (key, name) = (workspace.key(), workspace.label());
                ensure!(self.state.layout.unassign(&key), "{name} is not assigned");
                self.state.observe_active();
                Ok(format!("Unassigned {name}."))
            }
            Command::Forget { project } => {
                self.state.layout.forget(&project)?;
                self.state.observe_active();
                Ok(format!(
                    "Removed project {project}; COSMIC workspaces are unchanged."
                ))
            }
            Command::Go { project } => {
                self.state.current()?;
                let key = self
                    .state
                    .layout
                    .go(&project, |key| self.state.find(key).is_some())?;
                self.activate(key.clone(), SlideAxis::Horizontal)
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
                    Some(key) => self.activate(
                        key.clone(),
                        match direction {
                            Direction::Left | Direction::Right => SlideAxis::Horizontal,
                            Direction::Up | Direction::Down => SlideAxis::Vertical,
                        },
                    ),
                    None => Ok("No workspace in that direction.".into()),
                }
            }
        }
    }

    fn sync_order(&mut self, dry_run: bool) -> Result<String> {
        // New handles may have been created during the previous roundtrip.
        if !dry_run
            && self.state.cosmic_manager.is_some()
            && self.state.visible().any(|w| w.can_move.is_none())
        {
            self.queue.roundtrip(&mut self.state)?;
            self.state.ensure_online()?;
        }
        self.state.current()?;
        let current = self.state.ordered_workspaces()?;
        let ids: Vec<_> = current.iter().map(|w| w.serial).collect();
        let managed: Vec<_> = self
            .state
            .layout
            .projects
            .iter()
            .flat_map(|project| &project.members)
            .filter_map(|key| self.state.find(key).map(|w| w.serial))
            .collect();
        let plan = OrderPlan::new(&ids, &managed);
        if plan.moves.is_empty() {
            return Ok("COSMIC workspace order already matches.".into());
        }
        let by_id: HashMap<_, _> = current.iter().map(|w| (w.serial, *w)).collect();
        if dry_run {
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
        ensure!(
            self.state.cosmic_manager.is_some(),
            "this session doesn't expose COSMIC workspace reordering"
        );
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
        for (source, anchor) in requests {
            source.move_before(anchor, 0);
        }
        self.state
            .manager
            .as_ref()
            .context("workspace manager went away")?
            .commit();
        self.queue.roundtrip(&mut self.state)?;
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

    fn activate(&mut self, key: Key, axis: SlideAxis) -> Result<String> {
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
        let override_layout = match &self.slide {
            Some(slide) => slide.apply(axis)?,
            None => None,
        };
        if override_layout.is_some() {
            thread::sleep(CONFIG_SETTLE);
        }
        workspace.handle.activate();
        self.state
            .manager
            .as_ref()
            .context("workspace manager went away")?
            .commit();
        self.queue.roundtrip(&mut self.state)?;
        // Serialize experimental moves even when the requested axis already
        // matches. The next shortcut must not flip it during this animation.
        if self.slide.is_some() {
            thread::sleep(ANIMATION_HOLD);
        }
        drop(override_layout);
        ensure!(
            self.state.find(&key).is_some_and(Workspace::active),
            "COSMIC did not confirm activation of {name}"
        );
        Ok(format!("Switched to {name}."))
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
    project_changes: ProjectChanges,
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
        let current = self.current().ok().map(|w| (w.serial, w.key()));
        let active = current.map(|(serial, key)| (serial, self.layout.observe_active(key)));
        self.project_changes.observe(active);
    }

    fn status(&self, grid: bool) -> String {
        let mut text = String::new();
        if let Err(error) = self.current() {
            writeln!(text, "Navigation unavailable: {error}").unwrap();
        }
        if self.layout.projects.is_empty() {
            writeln!(
                text,
                "No projects yet. Run `cpw assign PROJECT` on the active workspace."
            )
            .unwrap();
        }
        if grid && !self.layout.projects.is_empty() {
            self.write_grid(&mut text);
        } else {
            for (column, project) in self.layout.projects.iter().enumerate() {
                writeln!(text, "{}. {}", column + 1, project.name).unwrap();
                for (row, key) in project.members.iter().enumerate() {
                    let label = match self.find(key) {
                        Some(w) if w.active() => format!("{} [active]", w.label()),
                        Some(w) => w.label(),
                        None => "(unavailable)".into(),
                    };
                    let last = if project.last.as_ref() == Some(key) {
                        " [last used]"
                    } else {
                        ""
                    };
                    let session = if matches!(key, Key::Session(_)) {
                        " [session only]"
                    } else {
                        ""
                    };
                    writeln!(text, "   {}. {label}{last}{session}", row + 1).unwrap();
                }
            }
        }
        let mut unassigned: Vec<_> = self
            .visible()
            .filter(|w| !self.layout.contains(&w.key()))
            .collect();
        unassigned.sort_by_key(|w| (w.coordinate, w.serial));
        if !unassigned.is_empty() {
            writeln!(text, "Unassigned:").unwrap();
            for w in unassigned {
                let active = if w.active() { " [active]" } else { "" };
                writeln!(text, "   {}{active}", w.label()).unwrap();
            }
        }
        text.trim_end().to_owned()
    }

    fn write_grid(&self, text: &mut String) {
        let columns: Vec<_> = self
            .layout
            .projects
            .iter()
            .map(|project| {
                let cells: Vec<_> = std::iter::once(project.name.clone())
                    .chain(project.members.iter().map(|key| {
                        let mut label = match self.find(key) {
                            Some(w) if w.active() => format!("[{}]", w.label()),
                            Some(w) => w.label(),
                            None => "(unavailable)".into(),
                        };
                        if project.last.as_ref() == Some(key) {
                            label.push_str(" *");
                        }
                        label
                    }))
                    .collect();
                let width = cells.iter().map(|cell| cell.width()).max().unwrap_or(0);
                (cells, width)
            })
            .collect();
        let height = columns
            .iter()
            .map(|(cells, _)| cells.len())
            .max()
            .unwrap_or(0);
        for row in 0..height {
            for (column, (cells, width)) in columns.iter().enumerate() {
                if column > 0 {
                    text.push_str(" | ");
                }
                let cell = cells.get(row).map_or("", String::as_str);
                text.push_str(cell);
                if column + 1 < columns.len() {
                    text.extend(std::iter::repeat_n(' ', width - cell.width()));
                }
            }
            text.push('\n');
        }
        writeln!(text, "[ ] active; * last used").unwrap();
    }
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
                    },
                );
            }
            manager::Event::Done => {
                state.ready = true;
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
                    state.layout.promote(w.key(), Key::Stable(id.clone()));
                    w.id = Some(id);
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
                if w.id.is_none() {
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
        if let cosmic_workspace::Event::Capabilities { capabilities } = event
            && let Some(workspace) = state.workspaces.get_mut(id)
        {
            workspace.can_move = Some(
                cosmic_workspace::WorkspaceCapabilities::from_bits_truncate(capabilities.into())
                    .contains(cosmic_workspace::WorkspaceCapabilities::Move),
            );
        }
    }
}
