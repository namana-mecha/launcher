use anyhow::Result;
use launcher::{
    renderer_thread::RendererEventManager,
    wayland_thread::WaylandEventManager,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    #[cfg(feature = "profile")]
    let _puffin_server = {
        puffin::set_scopes_on(true);
        let server_addr = format!("127.0.0.1:{}", puffin_http::DEFAULT_PORT);
        match puffin_http::Server::new(&server_addr) {
            Ok(server) => {
                eprintln!(
                    "Puffin HTTP server running on {server_addr}. \
                     Connect with: puffin_viewer --url {server_addr}"
                );
                Some(server)
            }
            Err(e) => {
                eprintln!("Failed to start Puffin server: {e}");
                None
            }
        }
    };

    const WIDTH:  u32 = 1028;
    const HEIGHT: u32 = 1080;

    // Create both managers (no I/O happens yet).
    let wayland_mgr  = WaylandEventManager::new(WIDTH, HEIGHT)?;
    let renderer_mgr = RendererEventManager::new(WIDTH, HEIGHT)?;

    // Exchange cross-thread handles before moving the managers into threads.
    let wayland_handle  = wayland_mgr.handle();
    let renderer_handle = renderer_mgr.handle();

    // Spawn dedicated threads.
    let wayland_thread = std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || wayland_mgr.run(renderer_handle))?;

    let renderer_thread = std::thread::Builder::new()
        .name("renderer".into())
        .spawn(move || renderer_mgr.run(wayland_handle))?;

    // Wait for both threads to finish.
    wayland_thread
        .join()
        .expect("wayland thread panicked")?;
    renderer_thread
        .join()
        .expect("renderer thread panicked")?;

    Ok(())
}
