//! Hoardini for Linux: the window and tray icon (HoardiniApp.swift).

mod manager;
mod model;
mod os;
mod setup;
mod tray;
mod ui;

use eframe::egui;
use hoardini_core::paths::{self, SingleInstance};

const ICON: &[u8] = include_bytes!("../../../packaging/icons/hoardini-256.png");

/// Asks the running instance to show its window (it watches the inbox).
fn ask_running_instance_to_show() {
    let inbox = paths::inbox();
    if paths::private_dir(&inbox).is_ok() {
        let _ = std::fs::write(inbox.join(format!("{}.show", uuid::Uuid::new_v4())), b"");
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("Hoardini {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // --background: started by the browser host; stay in the tray instead of opening the window.
    let background = args.iter().any(|a| a == "--background");
    let Some(_instance) = SingleInstance::acquire() else {
        if !background {
            ask_running_instance_to_show();
        }
        return Ok(());
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let manager = manager::Manager::new(rt.handle().clone());
    // Logging out or shutting down sends SIGTERM: pause the downloads cleanly so they resume next time.
    let m = manager.clone();
    rt.spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        m.request_quit();
    });

    let icon = image::load_from_memory(ICON).map(|i| i.to_rgba8()).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Hoardini")
        .with_app_id("hoardini")
        .with_inner_size([680.0, 440.0])
        .with_min_inner_size([520.0, 320.0]);
    if let Some(img) = &icon {
        viewport = viewport.with_icon(egui::IconData { rgba: img.as_raw().clone(), width: img.width(), height: img.height() });
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let icon_rgba = icon.map(|i| (i.width(), i.into_raw())).map(|(w, raw)| (raw, w)).unwrap_or_else(|| (vec![0; 4], 1));

    let m = manager.clone();
    let result = eframe::run_native(
        "Hoardini",
        options,
        Box::new(move |cc| Ok(Box::new(ui::HoardiniApp::new(cc, m, background, icon_rgba)))),
    );
    manager.shutdown();
    result
}
