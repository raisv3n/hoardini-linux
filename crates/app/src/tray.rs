//! Tray icon (the macOS menu bar extra), over StatusNotifierItem. Menu events arrive on the tray's
//! D-Bus thread; they only flip flags and wake the window's event loop, which acts on them.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use eframe::egui;
use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::{manager::Manager, ui::bytes};

pub const SHOW: u8 = 1;
pub const SETUP: u8 = 2;
pub const QUIT: u8 = 4;

pub struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    clear: MenuItem,
    last: (usize, u64, usize),
    /// Bits of SHOW / SETUP / QUIT requested from the menu.
    pub actions: Arc<AtomicU8>,
}

pub fn install(manager: Arc<Manager>, ctx: egui::Context, rgba: Vec<u8>, size: u32) -> Option<Tray> {
    let status = MenuItem::new("No active downloads", false, None);
    let clear = MenuItem::new("Clear Finished", false, None);
    let show = MenuItem::new("Show Hoardini", true, None);
    let setup = MenuItem::new("Set Up Firefox Extension…", true, None);
    let quit = MenuItem::new("Quit Hoardini", true, None);
    let menu = Menu::new();
    menu.append_items(&[&status, &PredefinedMenuItem::separator(), &clear, &show, &setup, &PredefinedMenuItem::separator(), &quit])
        .ok()?;
    let icon = TrayIconBuilder::new()
        .with_id("hoardini")
        .with_title("Hoardini")
        .with_tooltip("Hoardini")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(Icon::from_rgba(rgba, size, size).ok()?)
        .build()
        .ok()?;

    let actions = Arc::new(AtomicU8::new(0));
    let (clear_id, show_id, setup_id, quit_id) = (clear.id().clone(), show.id().clone(), setup.id().clone(), quit.id().clone());
    let a = actions.clone();
    let c = ctx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == clear_id {
            manager.clear_finished();
        } else if e.id == show_id {
            a.fetch_or(SHOW, Ordering::SeqCst);
        } else if e.id == setup_id {
            a.fetch_or(SHOW | SETUP, Ordering::SeqCst);
        } else if e.id == quit_id {
            a.fetch_or(QUIT, Ordering::SeqCst);
        }
        c.request_repaint();
    }));
    let a = actions.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, .. } = e {
            a.fetch_or(SHOW, Ordering::SeqCst);
            ctx.request_repaint();
        }
    }));
    Some(Tray { icon, status, clear, last: (usize::MAX, 0, 0), actions })
}

impl Tray {
    /// Updates the status line, the Clear Finished item and the tooltip. Window thread only.
    pub fn refresh(&mut self, manager: &Manager) {
        let (active, speed) = manager.activity();
        let finished = manager.finished_count();
        let key = (active, speed as u64 / 1024, finished);
        if self.last == key {
            return;
        }
        self.last = key;
        let line = if active == 0 { "No active downloads".to_owned() } else { format!("{active} downloading · {}/s", bytes(speed as u64)) };
        self.status.set_text(&line);
        self.clear.set_text(if finished > 0 { format!("Clear Finished ({finished})") } else { "Clear Finished".into() });
        self.clear.set_enabled(finished > 0);
        let _ = self.icon.set_tooltip(Some(if active == 0 { "Hoardini".to_owned() } else { format!("Hoardini · {line}") }));
    }
}
