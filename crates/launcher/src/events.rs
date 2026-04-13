use std::os::unix::io::OwnedFd;

use core::Event;

// ── Wayland → Renderer ─────────────────────────────────────────────────────

/// Wayland surface has been configured; the renderer may begin producing frames.
pub struct SurfaceReady;

/// A frame has been committed to the Wayland surface; render the next one.
pub struct RenderTick;

/// The window was closed; both threads should exit.
pub struct Shutdown;

// ── Renderer → Wayland ─────────────────────────────────────────────────────

/// A rendered frame is ready; the Wayland thread should attach it to the surface.
pub struct FrameReady {
    pub fd:       OwnedFd,
    pub stride:   u32,
    pub offset:   u32,
    pub format:   u32,
    pub modifier: u64,
    pub width:    i32,
    pub height:   i32,
}

impl Event for SurfaceReady {}
impl Event for RenderTick {}
impl Event for Shutdown {}
impl Event for FrameReady {}
