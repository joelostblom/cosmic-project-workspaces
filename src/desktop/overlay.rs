mod render;

use super::State;
use crate::{layout::ProjectPosition, project_changes::Change};
use anyhow::{Context, Result};
use render::BannerRenderer;
use rustix::fs::{MemfdFlags, memfd_create};
use std::{
    fs::File,
    io::Write,
    os::fd::AsFd,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::GlobalList,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_region::WlRegion,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

const SCALE: i32 = 2;
const DISPLAY_TIME: Duration = Duration::from_millis(1100);
const RIGHT_MARGIN: i32 = 12;

#[derive(Clone, Copy)]
enum Kind {
    Project,
    Workspaces,
}

impl Kind {
    const ALL: [Self; 2] = [Self::Project, Self::Workspaces];

    fn size(self) -> (u32, u32) {
        match self {
            Self::Project => (660, 204),
            // A fixed transparent canvas lets the dots grow and shrink in place.
            Self::Workspaces => (28, 384),
        }
    }

    fn namespace(self) -> &'static str {
        match self {
            Self::Project => "cpw-project-banner",
            Self::Workspaces => "cpw-workspace-dots",
        }
    }
}

#[derive(PartialEq, Eq)]
enum Content {
    Project(String),
    Workspaces { row: usize, total: usize },
}

impl Content {
    fn kind(&self) -> Kind {
        match self {
            Self::Project(_) => Kind::Project,
            Self::Workspaces { .. } => Kind::Workspaces,
        }
    }
}

pub(super) struct Overlay {
    compositor: WlCompositor,
    shell: ZwlrLayerShellV1,
    shm: WlShm,
    renderer: BannerRenderer,
    popups: [Option<Popup>; Kind::ALL.len()],
}

struct Popup {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    content: Content,
    deadline: Instant,
    configured_size: Option<(u32, u32)>,
}

impl Overlay {
    pub(super) fn bind(globals: &GlobalList, queue: &QueueHandle<State>) -> Result<Self> {
        Ok(Self {
            compositor: globals
                .bind(queue, 4..=4, ())
                .context("no Wayland compositor v4")?,
            shell: globals
                .bind(queue, 1..=1, ())
                .context("no Wayland layer shell")?,
            shm: globals
                .bind(queue, 1..=1, ())
                .context("no Wayland shared memory")?,
            renderer: BannerRenderer::new()?,
            popups: [None, None],
        })
    }

    pub(super) fn update(
        &mut self,
        change: Change,
        position: Option<&ProjectPosition>,
        queue: &QueueHandle<State>,
    ) {
        let Some(position) = position else {
            for kind in Kind::ALL {
                self.hide(kind);
            }
            return;
        };
        let dots = Content::Workspaces {
            row: position.row,
            total: position.total,
        };
        if change != Change::Layout {
            self.show(dots, queue);
        } else if let Some(popup) = &mut self.popups[Kind::Workspaces as usize]
            && popup.content != dots
        {
            // Update a visible indicator when a row disappears, without
            // extending its timer or popping it up on a quiet layout edit.
            popup.content = dots;
            self.redraw(Kind::Workspaces, queue);
        }
        if change == Change::Project {
            self.show(Content::Project(position.name.clone()), queue);
        } else {
            // Only the dots should remain visible when moving within a project.
            self.hide(Kind::Project);
        }
    }

    fn show(&mut self, content: Content, queue: &QueueHandle<State>) {
        let kind = content.kind();
        let slot = &mut self.popups[kind as usize];
        let deadline = Instant::now() + DISPLAY_TIME;
        if let Some(popup) = slot {
            popup.deadline = deadline;
            if popup.content != content {
                popup.content = content;
                self.redraw(kind, queue);
            }
        } else {
            let surface = self.compositor.create_surface(queue, ());
            let empty_region = self.compositor.create_region(queue, ());
            surface.set_input_region(Some(&empty_region));
            empty_region.destroy();
            surface.set_buffer_scale(SCALE);
            let layer = self.shell.get_layer_surface(
                &surface,
                None,
                Layer::Overlay,
                kind.namespace().into(),
                queue,
                kind,
            );
            match kind {
                Kind::Project => layer.set_anchor(Anchor::empty()),
                Kind::Workspaces => {
                    // A single right anchor leaves the surface centered vertically.
                    layer.set_anchor(Anchor::Right);
                    layer.set_margin(0, RIGHT_MARGIN, 0, 0);
                }
            }
            let (width, height) = kind.size();
            layer.set_size(width, height);
            layer.set_exclusive_zone(-1); // Position against the whole output, ignoring panels.
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit(); // An unbuffered commit asks for the first configure.
            *slot = Some(Popup {
                surface,
                layer,
                content,
                deadline,
                configured_size: None,
            });
        }
    }

    pub(super) fn expire(&mut self, now: Instant) {
        for kind in Kind::ALL {
            if self.popups[kind as usize]
                .as_ref()
                .is_some_and(|popup| now >= popup.deadline)
            {
                self.hide(kind);
            }
        }
    }

    pub(super) fn time_remaining(&self, now: Instant) -> Option<Duration> {
        self.popups
            .iter()
            .flatten()
            .map(|popup| popup.deadline.saturating_duration_since(now))
            .min()
    }

    fn hide(&mut self, kind: Kind) {
        if let Some(popup) = self.popups[kind as usize].take() {
            popup.layer.destroy();
            popup.surface.destroy();
        }
    }

    fn redraw(&mut self, kind: Kind, queue: &QueueHandle<State>) {
        if let Err(error) = self.paint(kind, queue) {
            eprintln!("Could not render {}: {error:#}", kind.namespace());
            self.hide(kind);
        }
    }

    fn paint(&mut self, kind: Kind, queue: &QueueHandle<State>) -> Result<()> {
        let Some(popup) = &self.popups[kind as usize] else {
            return Ok(());
        };
        let Some((width, height)) = popup.configured_size else {
            return Ok(());
        };
        let (width, height) = (width as i32 * SCALE, height as i32 * SCALE);
        let (w, h) = (width as usize, height as usize);
        let pixels = match &popup.content {
            Content::Project(title) => self.renderer.render(title, w, h),
            Content::Workspaces { row, total } => render::workspace_dots(*row, *total, w, h),
        };
        let mut file = File::from(memfd_create(kind.namespace(), MemfdFlags::CLOEXEC)?);
        file.write_all(&pixels)?;
        let pool = self
            .shm
            .create_pool(file.as_fd(), pixels.len() as i32, queue, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            queue,
            (),
        );
        pool.destroy();
        popup.surface.attach(Some(&buffer), 0, 0);
        popup.surface.damage_buffer(0, 0, width, height);
        popup.surface.commit();
        Ok(())
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ZwlrLayerShellV1);

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            buffer.destroy();
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, Kind> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &kind: &Kind,
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        let Some(overlay) = state.overlay.as_mut() else {
            return;
        };
        let Some(popup) = overlay.popups[kind as usize]
            .as_mut()
            .filter(|popup| &popup.layer == layer)
        else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                let size = |offered: u32, maximum: u32| {
                    if offered == 0 {
                        maximum
                    } else {
                        offered.min(maximum)
                    }
                };
                let (max_width, max_height) = kind.size();
                popup.configured_size = Some((size(width, max_width), size(height, max_height)));
                overlay.redraw(kind, queue);
            }
            zwlr_layer_surface_v1::Event::Closed => overlay.hide(kind),
            _ => {}
        }
    }
}
