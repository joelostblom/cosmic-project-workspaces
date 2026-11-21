use crate::{
    Command,
    layout::{Direction, Key, Layout, SavedLayout},
    notifications::ProjectChanges,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rustix::event::{PollFd, PollFlags, poll};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    io,
    os::unix::net::UnixListener,
};
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
}

impl Desktop {
    pub fn connect(layout: Layout) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to the COSMIC Wayland session")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let manager = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("this session does not expose ext_workspace_manager_v1")?;
        let mut state = State {
            manager: Some(manager),
            layout,
            ..Default::default()
        };
        queue.roundtrip(&mut state)?;
        state.ensure_online()?;
        ensure!(
            state.ready,
            "the compositor did not send a workspace snapshot"
        );
        Ok(Self { conn, queue, state })
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
        match poll(&mut fds, None) {
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
            Command::Status => Ok(self.state.status()),
            Command::Assign { project, top } => {
                let workspace = self.state.current()?;
                let (key, name) = (workspace.key(), workspace.label());
                let hint = if matches!(key, Key::Session(_)) {
                    "\nSession only unless you pin this workspace in COSMIC."
                } else {
                    ""
                };
                self.state.layout.assign(key, project.clone(), top)?;
                self.state.observe_active();
                Ok(format!("Assigned {name} to {project}.{hint}"))
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
                self.activate(key.clone())
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
                    Some(key) => self.activate(key.clone()),
                    None => Ok("No workspace in that direction.".into()),
                }
            }
        }
    }

    fn activate(&mut self, key: Key) -> Result<String> {
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
        workspace.handle.activate();
        self.state
            .manager
            .as_ref()
            .context("workspace manager went away")?
            .commit();
        self.queue.roundtrip(&mut self.state)?;
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
    ready: bool,
    groups: HashSet<ObjectId>,
    workspaces: HashMap<ObjectId, Workspace>,
    next_serial: u64,
    layout: Layout,
    project_changes: ProjectChanges,
}

struct Workspace {
    handle: ExtWorkspaceHandleV1,
    group: Option<ObjectId>,
    serial: u64,
    id: Option<String>,
    name: Option<String>,
    coordinate: Option<u32>,
    state: workspace::State,
    can_activate: bool,
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
        let active = current.map(|(serial, key)| {
            let project = self.layout.observe_active(key).map(str::to_owned);
            (serial, project)
        });
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
                "No projects yet. Run `cpw assign PROJECT` on the active workspace."
            )
            .unwrap();
        }
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
        _: &QueueHandle<Self>,
    ) {
        match event {
            manager::Event::WorkspaceGroup { workspace_group } => {
                state.groups.insert(workspace_group.id());
            }
            manager::Event::Workspace { workspace } => {
                state.next_serial += 1;
                state.workspaces.insert(
                    workspace.id(),
                    Workspace {
                        handle: workspace,
                        group: None,
                        serial: state.next_serial,
                        id: None,
                        name: None,
                        coordinate: None,
                        state: workspace::State::empty(),
                        can_activate: false,
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
                    .get(..4)
                    .map(|raw| u32::from_ne_bytes(raw.try_into().expect("a four-byte coordinate")));
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
                state.workspaces.remove(&handle.id());
                handle.destroy();
            }
            _ => {}
        }
    }
}
