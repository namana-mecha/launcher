#![allow(unused_variables, unused_mut, dead_code)]
use anyhow::Result;
use launcher::{profile_function, profile_scope};
use renderer::primitives::RenderablePrimitive as _;
use renderer::{Quad, Rect as RenderRect, Renderer, TextMetrics, TextSystem};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};
use utils::asset_manager::AssetManager;
use utils::font::FontAsset;
use wayland_protocols::connection::Connection;
use wayland_protocols::wl_callback::SyncCallback;
use wayland_protocols::wl_display::Display;
use wayland_protocols::wl_registry::Registry;
use wayland_protocols::xdg_surface::XdgSurf;
use wayland_protocols::xdg_toplevel::Toplevel;
use wayland_protocols::xdg_wm_base::WmBase;
use wayland_protocols::zwp_linux_dmabuf::DmaBuf;
use wayland_protocols::*;

use layout::{
    Dimension, Display as FlexDisplay, Edges, FlexDirection, Layout, LengthPercentage, Measure,
    NodeId, Rect as LayoutRect, Size, Style,
};

const WIDTH: u32 = 1028;
const HEIGHT: u32 = 1080;
const HEADER_H: f32 = 70.0;
const ROWS: usize = 3;
const COLS: usize = 4; // last col in each row uses flex_grow
const ROW_H: f32 = 330.0;
const GAP: f32 = 10.0;
// Cells per row that have an explicit animated width (the last one flex-grows)
const ANIM_COLS: usize = COLS - 1;

const CELL_COLORS: [[f32; 4]; 12] = [
    [1.0, 0.2, 0.3, 1.0],
    [1.0, 0.5, 0.1, 1.0],
    [0.9, 0.8, 0.1, 1.0],
    [0.2, 0.8, 0.3, 1.0],
    [0.1, 0.7, 0.9, 1.0],
    [0.2, 0.4, 1.0, 1.0],
    [0.6, 0.2, 1.0, 1.0],
    [1.0, 0.2, 0.8, 1.0],
    [1.0, 0.4, 0.2, 1.0],
    [0.2, 0.9, 0.6, 1.0],
    [0.4, 0.6, 1.0, 1.0],
    [1.0, 0.7, 0.3, 1.0],
];

// (flat cell index, label) — cell index = row * COLS + col
const TEXT_CELLS: [(usize, &str); 3] = [(1, "Mecha"), (5, "Launcher"), (9, "Demo")];

struct Widget {
    measured: Option<TextMetrics>,
}

impl Measure for Widget {
    fn measure(
        &self,
        _known: Size<Option<f32>>,
        _available: Size<layout::AvailableSpace>,
    ) -> Size<f32> {
        match &self.measured {
            Some(m) => Size {
                width: m.width,
                height: m.height(),
            },
            None => Size::ZERO,
        }
    }
}

fn no_measure() -> Widget {
    Widget { measured: None }
}
fn measured(m: TextMetrics) -> Widget {
    Widget { measured: Some(m) }
}

fn to_rect(r: LayoutRect) -> RenderRect {
    RenderRect {
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
    }
}

fn cell_style(w: f32) -> Style {
    Style {
        size: Size {
            width: Dimension::length(w),
            height: Dimension::length(ROW_H),
        },
        ..Default::default()
    }
}

fn grow_style() -> Style {
    Style {
        flex_grow: 1.0,
        size: Size {
            width: Dimension::auto(),
            height: Dimension::length(ROW_H),
        },
        ..Default::default()
    }
}

fn main() -> Result<()> {
    profile_function!();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    #[cfg(feature = "profile")]
    let _puffin_server = {
        puffin::set_scopes_on(true);
        let server_addr = format!("127.0.0.1:{}", puffin_http::DEFAULT_PORT);
        match puffin_http::Server::new(&server_addr) {
            Ok(server) => {
                eprintln!("Puffin HTTP server running on {server_addr}");
                Some(server)
            }
            Err(e) => {
                eprintln!("Failed to start Puffin server: {e}");
                None
            }
        }
    };

    // ── Wayland setup ─────────────────────────────────────────────────────────

    let mut conn = Connection::connect()?;

    let mut display = Display::new(1);
    let mut registry = Registry::new(conn.alloc_id());
    let mut sync = SyncCallback::new(conn.alloc_id());

    display.inner.get_registry(&mut conn, &registry.inner)?;
    display.inner.sync(&mut conn, &sync)?;
    conn.flush()?;

    loop {
        let (obj_id, opcode, body) = conn.recv_msg()?;
        dispatch_to!(conn, obj_id, opcode, &body; display, registry, sync);
        if sync.done {
            break;
        }
    }

    let (comp_name, comp_ver) = registry
        .find("wl_compositor")
        .expect("wl_compositor missing");
    let (xdg_name, _) = registry.find("xdg_wm_base").expect("xdg_wm_base missing");
    let (dmabuf_name, dmabuf_ver) = registry
        .find("zwp_linux_dmabuf_v1")
        .expect("zwp_linux_dmabuf_v1 missing");

    let compositor = WlCompositor::new(conn.alloc_id());
    let wm_inner = XdgWmBase::new(conn.alloc_id());
    let dmabuf_inner = ZwpLinuxDmabufV1::new(conn.alloc_id());

    registry.inner.bind(
        &mut conn,
        comp_name,
        "wl_compositor",
        comp_ver.min(4),
        &compositor,
    )?;
    registry
        .inner
        .bind(&mut conn, xdg_name, "xdg_wm_base", 1, &wm_inner)?;
    registry.inner.bind(
        &mut conn,
        dmabuf_name,
        "zwp_linux_dmabuf_v1",
        dmabuf_ver.min(4),
        &dmabuf_inner,
    )?;

    let mut wm_base = WmBase::new(wm_inner);
    let mut dmabuf = DmaBuf::new(dmabuf_inner);

    let mut surface = WlSurface::new(conn.alloc_id());
    let xdg_inner = XdgSurface::new(conn.alloc_id());
    let top_inner = XdgToplevel::new(conn.alloc_id());

    compositor.create_surface(&mut conn, &surface)?;
    wm_base
        .inner
        .get_xdg_surface(&mut conn, &xdg_inner, &surface)?;

    let mut xdg_surf = XdgSurf::new(xdg_inner);
    let mut toplevel = Toplevel::new(top_inner);

    xdg_surf.inner.get_toplevel(&mut conn, &toplevel.inner)?;
    toplevel.inner.set_title(&mut conn, "Mecha Launcher")?;
    toplevel.inner.set_app_id(&mut conn, "mecha-launcher")?;
    surface.commit(&mut conn)?;
    conn.flush()?;

    let mut renderer = Renderer::new(WIDTH, HEIGHT)?;
    renderer.register::<Quad>()?;
    renderer.register::<renderer::MonoSprite>()?;

    let mut assets = AssetManager::new();
    let font_handle = assets.load::<FontAsset, _>("assets/Inter-Regular.ttf")?;

    let mut text_sys = TextSystem::new(renderer.gl(), 1024)?;
    let font_id = text_sys.load_font(&assets.get(&font_handle).unwrap().data)?;

    // ── Pre-measure text ──────────────────────────────────────────────────────

    let header_m = text_sys.measure_text("Kitchen Sink", font_id, 36.0);
    let cell_text_ms: Vec<TextMetrics> = TEXT_CELLS
        .iter()
        .map(|(_, label)| text_sys.measure_text(label, font_id, 28.0))
        .collect();

    // ── Layout tree (structure fixed; cell widths updated every frame) ─────────
    //
    // Root (flex col)
    //   Header (fixed height)
    //   Row 0 (flex row, gap) → cell[0][0..2] + cell[0][3] (flex-grow)
    //   Row 1 (flex row, gap) → cell[1][0..2] + cell[1][3] (flex-grow)
    //   Row 2 (flex row, gap) → cell[2][0..2] + cell[2][3] (flex-grow)

    let initial_w = 180.0_f32;

    let (mut layout, (header_id, row_cell_ids)) = Layout::new(
        Style {
            display: FlexDisplay::Flex,
            flex_direction: FlexDirection::Column,
            gap: Size {
                width: LengthPercentage::length(GAP),
                height: LengthPercentage::length(GAP),
            },
            padding: Edges {
                top: LengthPercentage::length(0.0),
                right: LengthPercentage::length(GAP),
                bottom: LengthPercentage::length(GAP),
                left: LengthPercentage::length(GAP),
            },
            size: Size {
                width: Dimension::percent(1.0),
                height: Dimension::percent(1.0),
            },
            ..Default::default()
        },
        no_measure(),
        |b| {
            let header = b.leaf(
                Style {
                    size: Size {
                        width: Dimension::percent(1.0),
                        height: Dimension::length(HEADER_H),
                    },
                    ..Default::default()
                },
                measured(header_m),
            );

            let mut row_cell_ids = [[NodeId::new(0); COLS]; ROWS];

            for row in 0..ROWS {
                let (_, ids) = b.child(
                    Style {
                        display: FlexDisplay::Flex,
                        flex_direction: FlexDirection::Row,
                        gap: Size {
                            width: LengthPercentage::length(GAP),
                            height: LengthPercentage::length(0.0),
                        },
                        ..Default::default()
                    },
                    no_measure(),
                    |b| {
                        let mut ids = [NodeId::new(0); COLS];
                        for col in 0..ANIM_COLS {
                            ids[col] = b.leaf(cell_style(initial_w), no_measure());
                        }
                        // last column fills remaining row width
                        ids[ANIM_COLS] = b.leaf(grow_style(), no_measure());
                        ids
                    },
                );
                row_cell_ids[row] = ids;
            }

            (header, row_cell_ids)
        },
    );

    layout.compute(LayoutRect {
        x: 0.0,
        y: 0.0,
        w: WIDTH as f32,
        h: HEIGHT as f32,
    });

    let mut scene = renderer.create_scene();
    let render_surface = renderer.create_dmabuf_surface();
    let mut configured = false;
    let mut wl_buf: Option<WlBuffer> = None;

    let mut frame_count = 0u64;
    let mut last_fps_report = Instant::now();
    let start_time = Instant::now();

    loop {
        #[cfg(feature = "profile")]
        puffin::GlobalProfiler::lock().new_frame();

        profile_scope!("event_loop");

        while let Some((obj_id, opcode, body)) = conn.try_recv_msg()? {
            dispatch_to!(conn, obj_id, opcode, &body;
                display, registry, dmabuf, wm_base, xdg_surf, toplevel, surface);
        }

        if let Some(serial) = wm_base.pending_pong.take() {
            wm_base.inner.pong(&mut conn, serial)?;
        }

        if let Some(serial) = xdg_surf.pending_ack.take() {
            xdg_surf.inner.ack_configure(&mut conn, serial)?;
            configured = true;
        }

        if configured {
            if wl_buf.is_none() {
                profile_scope!("dmabuf_setup");
                let frame = renderer.present()?;
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
                    WIDTH as i32,
                    HEIGHT as i32,
                    frame.format,
                    0,
                )?;
                params.destroy(&mut conn)?;
                wl_buf = Some(buf);
            }

            let buf = wl_buf.as_ref().unwrap();

            {
                profile_scope!("render");

                let t = start_time.elapsed().as_secs_f32();

                // ── Update animated cell widths and recompute layout ───────────
                for row in 0..ROWS {
                    for col in 0..ANIM_COLS {
                        let flat = row * COLS + col;
                        let phase = flat as f32 * TAU / (ROWS * ANIM_COLS) as f32;
                        let w = 80.0 + 180.0 * ((t * 0.8 + phase).sin() * 0.5 + 0.5);
                        layout.set_style(row_cell_ids[row][col], cell_style(w));
                    }
                }
                layout.compute(LayoutRect {
                    x: 0.0,
                    y: 0.0,
                    w: WIDTH as f32,
                    h: HEIGHT as f32,
                });

                scene.clear_primitives();
                scene.background = (0.08, 0.08, 0.12);

                // ── Header background + title ─────────────────────────────────
                let hr = layout.rect(header_id);
                Quad {
                    bounds: to_rect(hr),
                    color: [0.05, 0.05, 0.10, 1.0],
                    clip_rect: None,
                }
                .add_to_scene(&mut scene);

                let hm = layout.data(header_id).measured.as_ref().unwrap();
                text_sys.draw_text(
                    &mut scene,
                    renderer.gl(),
                    "Kitchen Sink",
                    font_id,
                    36.0,
                    [1.0, 1.0, 1.0, 1.0],
                    [
                        hr.x + (hr.w - hm.width) / 2.0,
                        hr.y + (hr.h - hm.height()) / 2.0 + hm.ascent,
                    ],
                )?;

                // ── Grid cells ────────────────────────────────────────────────
                for row in 0..ROWS {
                    for col in 0..COLS {
                        let flat = row * COLS + col;
                        let cell_r = layout.rect(row_cell_ids[row][col]);
                        Quad {
                            bounds: to_rect(cell_r),
                            color: CELL_COLORS[flat],
                            clip_rect: None,
                        }
                        .add_to_scene(&mut scene);
                    }
                }

                // ── Text in selected cells ────────────────────────────────────
                for (idx, &(flat, label)) in TEXT_CELLS.iter().enumerate() {
                    let row = flat / COLS;
                    let col = flat % COLS;
                    let cell_r = layout.rect(row_cell_ids[row][col]);
                    let m = &cell_text_ms[idx];
                    // only draw text if it fits within the cell
                    if m.width <= cell_r.w && m.height() <= cell_r.h {
                        text_sys.draw_text(
                            &mut scene,
                            renderer.gl(),
                            label,
                            font_id,
                            28.0,
                            [1.0, 1.0, 1.0, 1.0],
                            [
                                cell_r.x + (cell_r.w - m.width) / 2.0,
                                cell_r.y + (cell_r.h - m.height()) / 2.0 + m.ascent,
                            ],
                        )?;
                    }
                }

                renderer.begin_frame(&render_surface, scene.background);
                renderer.render_primitive::<Quad>(&scene, &render_surface)?;
                renderer.render_primitive::<renderer::MonoSprite>(&scene, &render_surface)?;
                renderer.end_frame();
            }

            {
                profile_scope!("surface_commit");
                surface.attach(&mut conn, buf, 0, 0)?;
                surface.damage(&mut conn, 0, 0, WIDTH as i32, HEIGHT as i32)?;
                surface.commit(&mut conn)?;
            }

            frame_count += 1;
            let now = Instant::now();
            let since_last = now.duration_since(last_fps_report);
            if since_last >= Duration::from_secs(60) {
                let fps = frame_count as f64 / since_last.as_secs_f64();
                tracing::info!(fps = format!("{:.1}", fps), "FPS report");
                frame_count = 0;
                last_fps_report = now;
            }
        }

        if toplevel.closed {
            tracing::info!("window closed");
            break;
        }

        conn.flush()?;
    }

    if let Some(buf) = wl_buf {
        buf.destroy(&mut conn)?;
        conn.flush()?;
    }

    Ok(())
}
