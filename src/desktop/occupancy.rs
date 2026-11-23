use super::State;
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self as cosmic_window, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
};
use std::collections::{HashMap, HashSet};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, backend::ObjectId, event_created_child,
    globals::GlobalList,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self as foreign_window, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self as foreign_list, ExtForeignToplevelListV1},
};

// Only retain window/workspace membership, not titles, app IDs, or geometry.
// The workspace protocol alone can't tell when a top row gains a window:
// moving that row up triggers a new trailing spare even if it stays empty.
#[derive(Default)]
pub(super) struct Occupancy {
    info: Option<ZcosmicToplevelInfoV1>,
    list: Option<ExtForeignToplevelListV1>,
    windows: HashMap<ObjectId, Window>,
}

struct Window {
    handle: ZcosmicToplevelHandleV1,
    workspaces: HashSet<ObjectId>,
    snapshot: Snapshot,
}

#[derive(PartialEq, Eq)]
enum Snapshot {
    AwaitingState,
    AwaitingDone,
    Ready,
}

impl Occupancy {
    pub(super) fn bind(globals: &GlobalList, queue: &QueueHandle<State>) -> Self {
        let info = globals.bind(queue, 3..=3, ()).ok();
        let list = info
            .as_ref()
            .and_then(|_| globals.bind(queue, 1..=1, ()).ok());
        Self {
            info,
            list,
            windows: HashMap::new(),
        }
    }

    pub(super) fn has_window(&self, workspace: &ObjectId) -> bool {
        self.windows
            .values()
            .any(|window| window.workspaces.contains(workspace))
    }

    pub(super) fn is_known_empty(&self, workspace: &ObjectId) -> bool {
        // A just-announced window doesn't have workspace membership yet.
        self.list.is_some()
            && self.windows.values().all(|window| {
                window.snapshot == Snapshot::Ready && !window.workspaces.contains(workspace)
            })
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelInfoV1,
        _: <ZcosmicToplevelInfoV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    event_created_child!(State, ExtForeignToplevelListV1, [
        foreign_list::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: foreign_list::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let foreign_list::Event::Toplevel { toplevel } = event
            && let Some(info) = &state.occupancy.info
        {
            let id = toplevel.id();
            let handle = info.get_cosmic_toplevel(&toplevel, queue, id.clone());
            state.occupancy.windows.insert(
                id,
                Window {
                    handle,
                    workspaces: HashSet::new(),
                    snapshot: Snapshot::AwaitingState,
                },
            );
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        window: &ExtForeignToplevelHandleV1,
        event: foreign_window::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            foreign_window::Event::Closed => {
                if let Some(entry) = state.occupancy.windows.remove(&window.id()) {
                    entry.handle.destroy();
                }
                window.destroy();
            }
            foreign_window::Event::Done => {
                if let Some(entry) = state.occupancy.windows.get_mut(&window.id())
                    && entry.snapshot == Snapshot::AwaitingDone
                {
                    entry.snapshot = Snapshot::Ready;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelHandleV1, ObjectId> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicToplevelHandleV1,
        event: cosmic_window::Event,
        id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(window) = state.occupancy.windows.get_mut(id) else {
            return;
        };
        match event {
            cosmic_window::Event::State { .. } if window.snapshot == Snapshot::AwaitingState => {
                window.snapshot = Snapshot::AwaitingDone;
            }
            cosmic_window::Event::ExtWorkspaceEnter { workspace } => {
                window.workspaces.insert(workspace.id());
            }
            cosmic_window::Event::ExtWorkspaceLeave { workspace } => {
                window.workspaces.remove(&workspace.id());
            }
            _ => {}
        }
    }
}
