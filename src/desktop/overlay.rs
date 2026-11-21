mod render;

use super::State;
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

const WIDTH: u32 = 660;
const HEIGHT: u32 = 204;
const SCALE: i32 = 2;
const DISPLAY_TIME: Duration = Duration::from_millis(1100);

pub(super) struct Overlay {
    compositor: WlCompositor,
    shell: ZwlrLayerShellV1,
    shm: WlShm,
    renderer: BannerRenderer,
    banner: Option<Banner>,
}

struct Banner {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    title: String,
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
            banner: None,
        })
    }

    pub(super) fn show(&mut self, title: &str, queue: &QueueHandle<State>) -> Result<()> {
        if let Some(banner) = self.banner.as_mut() {
            banner.title = title.to_owned();
            banner.deadline = Instant::now() + DISPLAY_TIME;
            if let Err(error) = self.paint(queue) {
                self.hide();
                return Err(error);
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
                "cpw-project-banner".into(),
                queue,
                (),
            );
            layer.set_anchor(Anchor::empty()); // Center on both axes.
            layer.set_size(WIDTH, HEIGHT);
            layer.set_exclusive_zone(-1); // Center on the output, not the area within panels.
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit(); // An unbuffered commit asks for the first configure.
            self.banner = Some(Banner {
                surface,
                layer,
                title: title.to_owned(),
                deadline: Instant::now() + DISPLAY_TIME,
                configured_size: None,
            });
        }
        Ok(())
    }

    pub(super) fn expire(&mut self, now: Instant) {
        if self
            .banner
            .as_ref()
            .is_some_and(|banner| now >= banner.deadline)
        {
            self.hide();
        }
    }

    pub(super) fn time_remaining(&self, now: Instant) -> Option<Duration> {
        self.banner
            .as_ref()
            .map(|banner| banner.deadline.saturating_duration_since(now))
    }

    fn hide(&mut self) {
        if let Some(banner) = self.banner.take() {
            banner.layer.destroy();
            banner.surface.destroy();
        }
    }

    fn paint(&mut self, queue: &QueueHandle<State>) -> Result<()> {
        let Some(banner) = self.banner.as_ref() else {
            return Ok(());
        };
        let Some((width, height)) = banner.configured_size else {
            return Ok(());
        };
        let (width, height) = (width as i32 * SCALE, height as i32 * SCALE);
        let pixels = self
            .renderer
            .render(&banner.title, width as usize, height as usize);
        let mut file = File::from(memfd_create("cpw-project-banner", MemfdFlags::CLOEXEC)?);
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
        banner.surface.attach(Some(&buffer), 0, 0);
        banner.surface.damage_buffer(0, 0, width, height);
        banner.surface.commit();
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

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        let Some(overlay) = state.overlay.as_mut() else {
            return;
        };
        let Some(banner) = overlay
            .banner
            .as_mut()
            .filter(|banner| &banner.layer == layer)
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
                banner.configured_size = Some((size(width, WIDTH), size(height, HEIGHT)));
                if let Err(error) = overlay.paint(queue) {
                    eprintln!("Could not render the project overlay: {error:#}");
                    overlay.hide();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => overlay.hide(),
            _ => {}
        }
    }
}
