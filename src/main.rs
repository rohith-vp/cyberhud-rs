//! CyberHUD: a native HUD wallpaper for Wayland compositors that support
//! wlr-layer-shell (KDE Plasma, Sway, Hyprland, ...).
//!
//! Everything is read directly from the system: /proc and /sys for
//! telemetry, MPRIS over D-Bus for track info, and the PulseAudio/PipeWire
//! default output monitor for the spectrum. No browser and no helper process.

mod audio;
mod canvas;
mod geo;
mod stats_debug;
mod music;
mod scene;
mod telemetry;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    reexports::calloop_wayland_source::WaylandSource,
    shell::{
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use smithay_client_toolkit::reexports::client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Time between redraws (about 30 fps). The terrain is cached and only
/// refreshed about 15 times a second, so the extra frames are cheap.
const FRAME: Duration = Duration::from_millis(33);

struct App {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    /// Size of the output in physical pixels (the current mode).
    phys: Option<(u32, u32)>,
    /// Maps the physical-size buffer onto the logical surface size.
    viewport: Option<WpViewport>,
    scene: scene::Scene,
    exit: bool,
    debug_since: std::time::Instant,
    debug_frames: u64,
    debug_draw_ms: f64,
    debug_draw_max: f64,
}

impl App {
    /// Current output mode in physical pixels, if the output reports one.
    fn physical_size(&self) -> Option<(u32, u32)> {
        self.output
            .outputs()
            .find_map(|o| self.output.info(&o))
            .and_then(|info| info.modes.iter().find(|m| m.current).map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32)))
    }

    fn draw(&mut self) {
        let frame_start = std::time::Instant::now();
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return;
        }
        // Draw at physical resolution when the viewport can scale it to the logical size.
        // Otherwise draw at logical resolution, as before.
        let (bw, bh) = match (&self.viewport, self.phys) {
            (Some(_), Some(p)) => p,
            _ => (w, h),
        };
        if self.pool.is_none() {
            let pool = SlotPool::new((bw * bh * 4 * 2) as usize, &self.shm).expect("create shm pool");
            self.pool = Some(pool);
        }
        self.scene.render(bw as usize, bh as usize);

        let pool = self.pool.as_mut().expect("pool exists");
        let stride = (bw * 4) as i32;
        let (buffer, canvas) = pool
            .create_buffer(bw as i32, bh as i32, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");
        // Our pixels are 0xAARRGGBB, which is the Argb8888 layout on little-endian,
        // so the buffer can be copied as raw bytes.
        #[cfg(target_endian = "little")]
        {
            let px = &self.scene.canvas.px;
            let bytes = unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
            // The pool can return a slightly larger buffer (it rounds allocations up),
            // so copy only the bytes we have.
            canvas[..bytes.len()].copy_from_slice(bytes);
        }
        #[cfg(target_endian = "big")]
        for (dst, src) in canvas.chunks_exact_mut(4).zip(self.scene.canvas.px.iter()) {
            dst.copy_from_slice(&src.to_le_bytes());
        }
        let surface = self.layer.wl_surface();
        buffer.attach_to(surface).expect("attach buffer");
        surface.damage_buffer(0, 0, bw as i32, bh as i32);
        surface.commit();

        if stats_debug::enabled() {
            self.debug_frames += 1;
            let took = frame_start.elapsed().as_secs_f64() * 1000.0;
            self.debug_draw_ms += took;
            if took > self.debug_draw_max {
                self.debug_draw_max = took;
            }
            if self.debug_since.elapsed() >= Duration::from_secs(5) {
                let secs = self.debug_since.elapsed().as_secs_f64();
                eprintln!(
                    "[cyberhud] fps {:.1} | draw avg {:.1} ms max {:.1} ms | bg rebuilds {} | spectrum updates {}/s | changed {}/s | max gap {:.0} ms",
                    self.debug_frames as f64 / secs,
                    self.debug_draw_ms / self.debug_frames.max(1) as f64,
                    self.debug_draw_max,
                    stats_debug::take(&stats_debug::BG_REBUILDS),
                    (stats_debug::take(&stats_debug::BAND_UPDATES) as f64 / secs) as u64,
                    (stats_debug::take(&stats_debug::BAND_CHANGES) as f64 / secs) as u64,
                    stats_debug::take(&stats_debug::MAX_GAP_US) as f64 / 1000.0,
                );
                self.debug_frames = 0;
                self.debug_draw_ms = 0.0;
                self.debug_draw_max = 0.0;
                self.debug_since = std::time::Instant::now();
            }
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        self.width = if w == 0 { 1920 } else { w };
        self.height = if h == 0 { 1080 } else { h };
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.width as i32, self.height as i32);
        }
        if self.phys.is_none() {
            self.phys = self.physical_size();
        }
        self.draw();
    }
}

// The viewport objects have no events we need.
impl Dispatch<WpViewporter, ()> for App {
    fn event(_: &mut Self, _: &WpViewporter, _: <WpViewporter as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewport, ()> for App {
    fn event(_: &mut Self, _: &WpViewport, _: <WpViewport as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Shared state written by background threads and read by the renderer.
    let stats = Arc::new(Mutex::new(telemetry::Stats::default()));
    let media = Arc::new(Mutex::new(music::Media::default()));
    let bands = Arc::new(Mutex::new(vec![0.0f32; audio::BANDS]));
    let latency = Arc::new(Mutex::new(None::<f64>));
    // CYBERHUD_FORCE_PLAYING=1 starts audio capture without a media player, for testing.
    let force = std::env::var_os("CYBERHUD_FORCE_PLAYING").is_some();
    let playing = Arc::new(AtomicBool::new(force));

    {
        let stats = Arc::clone(&stats);
        let latency = Arc::clone(&latency);
        thread::spawn(move || {
            let mut collector = telemetry::Collector::new();
            loop {
                let online = latency.lock().unwrap().is_some();
                let sample = collector.sample(online);
                *stats.lock().unwrap() = sample;
                thread::sleep(Duration::from_secs(1));
            }
        });
    }
    {
        let latency = Arc::clone(&latency);
        thread::spawn(move || loop {
            let value = telemetry::measure_latency();
            *latency.lock().unwrap() = value;
            thread::sleep(Duration::from_secs(15));
        });
    }
    music::spawn(Arc::clone(&media), Arc::clone(&playing), force);
    audio::spawn(Arc::clone(&bands), Arc::clone(&playing));

    // Wayland: a layer-shell surface on the background layer covers the screen.
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), event_queue).insert(event_loop.handle())?;

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Background, Some("cyberhud"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(0, 0);

    // Optional: scale the physical-size buffer to the logical surface size.
    let viewport = match globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()) {
        Ok(viewporter) => Some(viewporter.get_viewport(layer.wl_surface(), &qh, ())),
        Err(_) => None,
    };
    layer.commit();

    let mut app = App {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm,
        layer,
        pool: None,
        width: 0,
        height: 0,
        phys: None,
        viewport,
        scene: scene::Scene::new(
            1920,
            1080,
            scene::Shared {
                stats: Arc::clone(&stats),
                media: Arc::clone(&media),
                bands: Arc::clone(&bands),
                latency: Arc::clone(&latency),
            },
        ),
        exit: false,
        debug_since: std::time::Instant::now(),
        debug_frames: 0,
        debug_draw_ms: 0.0,
        debug_draw_max: 0.0,
    };

    event_loop
        .handle()
        .insert_source(Timer::immediate(), |deadline, _, app: &mut App| {
            app.draw();
            // Schedule from the previous deadline, so a slow frame doesn't drift the cadence.
            TimeoutAction::ToInstant(deadline + FRAME)
        })?;
    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }
    Ok(())
}
