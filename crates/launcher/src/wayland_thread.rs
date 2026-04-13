use anyhow::Result;
use core::{EventManager, EventManagerHandle};
use wayland_protocols::*;
use wayland_protocols::{
    connection::Connection,
    wl_callback::SyncCallback,
    wl_display::Display,
    wl_registry::Registry,
    xdg_surface::XdgSurf,
    xdg_toplevel::Toplevel,
    xdg_wm_base::WmBase,
    zwp_linux_dmabuf::DmaBuf,
};

use crate::events::{FrameReady, RenderTick, Shutdown, SurfaceReady};

/// Manages all Wayland protocol I/O.
///
/// All socket operations happen inside [`run`](WaylandEventManager::run) on
/// the dedicated Wayland thread. The `EventManagerHandle` returned by
/// [`handle`](WaylandEventManager::handle) lets the renderer thread send
/// [`FrameReady`] events here.
pub struct WaylandEventManager {
    em: EventManager,
    width: u32,
    height: u32,
}

// SAFETY: WaylandEventManager is only used from the Wayland thread after
// it is spawned. The Connection inside run() is created on that thread.
unsafe impl Send for WaylandEventManager {}

impl WaylandEventManager {
    /// Create a new manager.  No Wayland I/O happens here.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let mut em = EventManager::new();
        em.register_event::<FrameReady>();
        Ok(Self { em, width, height })
    }

    /// Return a handle that other threads can use to send events to this manager.
    pub fn handle(&self) -> EventManagerHandle {
        self.em.handle()
    }

    /// Connect to the Wayland compositor, create the window, and run the event
    /// loop until the window is closed.  Blocks until the manager shuts down.
    pub fn run(self, renderer_handle: EventManagerHandle) -> Result<()> {
        let width  = self.width;
        let height = self.height;
        let mut em = self.em;

        // ── Connect & initial sync round-trip ─────────────────────────────
        let mut conn = Connection::connect()?;

        let mut display  = Display::new(1);
        let mut registry = Registry::new(conn.alloc_id());
        let mut sync     = SyncCallback::new(conn.alloc_id());

        display.inner.get_registry(&mut conn, &registry.inner)?;
        display.inner.sync(&mut conn, &sync)?;
        conn.flush()?;

        tracing::info!("[wayland] waiting for globals");

        loop {
            let (obj_id, opcode, body) = conn.recv_msg()?;
            dispatch_to!(conn, obj_id, opcode, &body; display, registry, sync);
            if sync.done {
                break;
            }
        }

        tracing::info!("[wayland] sync complete, binding globals");

        // ── Bind globals ──────────────────────────────────────────────────
        let (comp_name, comp_ver) = registry
            .find("wl_compositor")
            .expect("wl_compositor missing");
        let (xdg_name, _) = registry
            .find("xdg_wm_base")
            .expect("xdg_wm_base missing");
        let (dmabuf_name, dmabuf_ver) = registry
            .find("zwp_linux_dmabuf_v1")
            .expect("zwp_linux_dmabuf_v1 missing");

        let compositor  = WlCompositor::new(conn.alloc_id());
        let wm_inner    = XdgWmBase::new(conn.alloc_id());
        let dmabuf_inner = ZwpLinuxDmabufV1::new(conn.alloc_id());

        registry.inner.bind(&mut conn, comp_name, "wl_compositor",      comp_ver.min(4),   &compositor)?;
        registry.inner.bind(&mut conn, xdg_name,  "xdg_wm_base",        1,                 &wm_inner)?;
        registry.inner.bind(&mut conn, dmabuf_name, "zwp_linux_dmabuf_v1", dmabuf_ver.min(4), &dmabuf_inner)?;

        let mut wm_base = WmBase::new(wm_inner);
        let mut dmabuf  = DmaBuf::new(dmabuf_inner);

        // ── Create surface ────────────────────────────────────────────────
        let mut surface  = WlSurface::new(conn.alloc_id());
        let xdg_inner    = XdgSurface::new(conn.alloc_id());
        let top_inner    = XdgToplevel::new(conn.alloc_id());

        compositor.create_surface(&mut conn, &surface)?;
        wm_base.inner.get_xdg_surface(&mut conn, &xdg_inner, &surface)?;

        let mut xdg_surf = XdgSurf::new(xdg_inner);
        let mut toplevel = Toplevel::new(top_inner);

        xdg_surf.inner.get_toplevel(&mut conn, &toplevel.inner)?;
        toplevel.inner.set_title(&mut conn, "hello wayland")?;
        toplevel.inner.set_app_id(&mut conn, "mecha-wayland")?;
        surface.commit(&mut conn)?;
        conn.flush()?;

        tracing::info!("[wayland] surface created, entering event loop");

        // ── Main event loop ───────────────────────────────────────────────
        let mut wl_buf: Option<WlBuffer> = None;
        let mut configured = false;

        loop {
            // ── Process FrameReady events from the renderer thread ────────
            for frame in em.drain_typed::<FrameReady>() {
                if !configured {
                    continue;
                }

                // Create the wl_buffer the first time we receive a frame.
                if wl_buf.is_none() {
                    let params = ZwpLinuxBufferParamsV1::new(conn.alloc_id());
                    dmabuf.inner.create_params(&mut conn, &params)?;

                    let mod_hi = (frame.modifier >> 32) as u32;
                    let mod_lo = frame.modifier as u32;
                    params.add(
                        &mut conn,
                        frame.fd,
                        0,
                        frame.offset,
                        frame.stride,
                        mod_hi,
                        mod_lo,
                    )?;

                    let buf = WlBuffer::new(conn.alloc_id());
                    params.create_immed(
                        &mut conn,
                        &buf,
                        frame.width,
                        frame.height,
                        frame.format,
                        0,
                    )?;
                    params.destroy(&mut conn)?;
                    wl_buf = Some(buf);
                }

                if let Some(buf) = &wl_buf {
                    surface.attach(&mut conn, buf, 0, 0)?;
                    surface.damage(&mut conn, 0, 0, width as i32, height as i32)?;
                    surface.commit(&mut conn)?;
                    conn.flush()?;
                }

                // Tell the renderer to produce the next frame.
                renderer_handle.send_event(RenderTick);
            }

            // ── Drain Wayland protocol events (non-blocking) ──────────────
            while let Some((obj_id, opcode, body)) = conn.try_recv_msg()? {
                dispatch_to!(conn, obj_id, opcode, &body;
                    display, registry, dmabuf, wm_base, xdg_surf, toplevel, surface);
            }

            // ── Respond to compositor ping ────────────────────────────────
            if let Some(serial) = wm_base.pending_pong.take() {
                wm_base.inner.pong(&mut conn, serial)?;
                conn.flush()?;
            }

            // ── Acknowledge surface configure ─────────────────────────────
            if let Some(serial) = xdg_surf.pending_ack.take() {
                xdg_surf.inner.ack_configure(&mut conn, serial)?;
                conn.flush()?;

                if !configured {
                    configured = true;
                    tracing::info!("[wayland] surface configured, signalling renderer");
                    renderer_handle.send_event(SurfaceReady);
                }
            }

            conn.flush()?;

            // ── Check for window close ────────────────────────────────────
            if toplevel.closed {
                tracing::info!("[wayland] window closed");
                renderer_handle.send_event(Shutdown);
                break;
            }
        }

        if let Some(buf) = wl_buf {
            buf.destroy(&mut conn)?;
            conn.flush()?;
        }

        Ok(())
    }
}
