use std::time::Instant;

use anyhow::Result;
use core::{EventManager, EventManagerHandle};
use renderer::{
    Image, MonoSprite, Quad, Rect, Renderer, TextSystem,
    primitives::RenderablePrimitive as _,
};
use utils::{font::FontAsset, image::ImageAsset, asset_manager::Asset};

use crate::events::{FrameReady, RenderTick, Shutdown, SurfaceReady};

/// Manages GPU rendering.
///
/// All EGL / OpenGL calls happen inside [`run`](RendererEventManager::run) on
/// the dedicated renderer thread. The `EventManagerHandle` returned by
/// [`handle`](RendererEventManager::handle) lets the Wayland thread send
/// control events (`SurfaceReady`, `RenderTick`, `Shutdown`) here.
pub struct RendererEventManager {
    em: EventManager,
    width: u32,
    height: u32,
}

impl RendererEventManager {
    /// Create a new manager. No GPU initialisation happens here.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let mut em = EventManager::new();
        em.register_event::<SurfaceReady>();
        em.register_event::<RenderTick>();
        em.register_event::<Shutdown>();
        Ok(Self { em, width, height })
    }

    /// Return a handle that other threads can use to send events to this manager.
    pub fn handle(&self) -> EventManagerHandle {
        self.em.handle()
    }

    /// Initialise the GPU on this thread, then enter the render loop.
    /// Blocks until a `Shutdown` event is received.
    pub fn run(self, wayland_handle: EventManagerHandle) -> Result<()> {
        let width  = self.width;
        let height = self.height;
        let mut em = self.em;

        // ── GPU initialisation (must happen on this thread) ───────────────
        let mut renderer = Renderer::new(width, height)?;
        renderer.register::<Quad>()?;
        renderer.register::<MonoSprite>()?;
        renderer.register::<Image>()?;

        let mut text_sys = TextSystem::new(renderer.gl(), 1024)?;
        let font_asset   = FontAsset::load("assets/Inter-Regular.ttf".into())?;
        let font_id      = text_sys.load_font(&font_asset.data)?;

        let image_asset = ImageAsset::load("assets/logo.png".into())?;
        // Upload directly — avoids keeping AssetManager alive.
        let logo   = renderer.upload_image(&image_asset)?;
        let logo_w = logo.width as f32;
        let logo_h = logo.height as f32;
        let logo_tex = logo.id();

        let mut scene          = renderer.create_scene();
        let     render_surface = renderer.create_dmabuf_surface();

        tracing::info!("[renderer] GPU ready, waiting for surface");

        // ── Animation / timing state ──────────────────────────────────────
        let start   = Instant::now();
        let mut configured      = false;
        let mut rect_x          = 100.0f32;
        let mut rect_y          = 100.0f32;
        let mut vel_x           = 120.0f32;
        let mut vel_y           = 90.0f32;
        let mut last_update     = Instant::now();
        let mut frame_count: u64 = 0;
        let mut last_fps_report  = Instant::now();

        // ── Render loop ───────────────────────────────────────────────────
        loop {
            // Block until the Wayland thread sends us something.
            em.wait_for_pending();

            // Shutdown takes priority.
            if !em.drain_typed::<Shutdown>().is_empty() {
                tracing::info!("[renderer] shutdown received");
                break;
            }

            // SurfaceReady acts as the first implicit RenderTick.
            let got_ready = !em.drain_typed::<SurfaceReady>().is_empty();
            if got_ready {
                configured = true;
                tracing::info!("[renderer] surface ready, starting render loop");
            }

            // Consume any pending ticks (there will normally be exactly one).
            em.drain_typed::<RenderTick>();

            if !configured {
                continue;
            }

            // ── Build scene ───────────────────────────────────────────────
            let t   = start.elapsed().as_secs_f32();
            let hue = (t * 72.0) % 360.0;
            let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

            let rect_w = 200.0f32;
            let rect_h = 100.0f32;

            let now = Instant::now();
            let mut dt = now.duration_since(last_update).as_secs_f32();
            if dt > 0.1 { dt = 0.1; }
            last_update = now;

            rect_x += vel_x * dt;
            rect_y += vel_y * dt;

            let max_x = width  as f32 - rect_w;
            let max_y = height as f32 - 80.0 - rect_h;

            if rect_x <= 0.0        { rect_x = 0.0;  vel_x =  vel_x.abs(); }
            else if rect_x >= max_x { rect_x = max_x; vel_x = -vel_x.abs(); }
            if rect_y <= 0.0        { rect_y = 0.0;  vel_y =  vel_y.abs(); }
            else if rect_y >= max_y { rect_y = max_y; vel_y = -vel_y.abs(); }

            scene.clear_primitives();
            scene.background = (1.0, 1.0, 1.0);

            // Bouncing coloured rectangle
            Quad {
                bounds: Rect { x: rect_x, y: rect_y, w: rect_w, h: rect_h },
                color:  [r, g, b, 1.0],
                clip_rect: None,
            }
            .add_to_scene(&mut scene);

            // Static bottom bar
            Quad {
                bounds: Rect {
                    x: 0.0,
                    y: (height - 80) as f32,
                    w: width as f32,
                    h: 80.0,
                },
                color:     [1.0, 0.0, 0.0, 0.8],
                clip_rect: None,
            }
            .add_to_scene(&mut scene);

            // Text label inside the bouncing rectangle
            text_sys.draw_text(
                &mut scene,
                renderer.gl(),
                "Hello, Wayland!",
                font_id,
                24.0,
                [1.0, 1.0, 1.0, 1.0],
                [rect_x + 10.0, rect_y + 60.0],
            )?;

            // Logo in the top-left corner
            Image {
                bounds:    Rect { x: 20.0, y: 20.0, w: logo_w, h: logo_h },
                texture:   logo_tex,
                clip_rect: None,
            }
            .add_to_scene(&mut scene);

            // ── Render ────────────────────────────────────────────────────
            renderer.begin_frame(&render_surface, scene.background);
            renderer.render_primitive::<Quad>(&scene, &render_surface)?;
            renderer.render_primitive::<MonoSprite>(&scene, &render_surface)?;
            renderer.render_primitive::<Image>(&scene, &render_surface)?;
            renderer.end_frame();

            // ── Present ───────────────────────────────────────────────────
            let frame = renderer.present()?;
            wayland_handle.send_event(FrameReady {
                fd:       frame.fd,
                stride:   frame.stride,
                offset:   frame.offset,
                format:   frame.format,
                modifier: frame.modifier,
                width:    width  as i32,
                height:   height as i32,
            });

            // ── FPS reporting ─────────────────────────────────────────────
            frame_count += 1;
            let since_last = now.duration_since(last_fps_report);
            if since_last >= std::time::Duration::from_secs(60) {
                let fps = frame_count as f64 / since_last.as_secs_f64();
                tracing::info!(fps = format!("{:.1}", fps), "[renderer] FPS report");
                frame_count      = 0;
                last_fps_report  = now;
            }
        }

        // Keep logo texture alive until the loop exits.
        drop(logo);
        Ok(())
    }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h = h % 360.0;
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (r1 + m, g1 + m, b1 + m)
}
