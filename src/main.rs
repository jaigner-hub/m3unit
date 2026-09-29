//! M3UNIT — a Winamp-flavored desktop player that streams M3U playlists from the internet.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod archive;
mod eq;
mod player;
mod playlist;
mod viz;

fn main() -> eframe::Result {
    // reqwest is built without a bundled crypto provider so the Windows build
    // needs no CMake/NASM; install the pure-Rust `ring` provider process-wide.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("m3unit-io")
        .build()
        .expect("failed to start tokio runtime");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("M3UNIT")
            .with_inner_size([app::WIN_W, app::WIN_H])
            .with_min_inner_size([app::WIN_W, app::MIN_H])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "m3unit",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, rt)))),
    )
}
