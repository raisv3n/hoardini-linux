//! The window (Views.swift): a toolbar, the download list with per-item controls, the Add URL sheet,
//! and the one-time "add the Firefox extension" dialog.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Color32, FontId, Id, Key, KeyboardShortcut, Layout, Modal, Modifiers, RichText, ScrollArea, Sense,
    Stroke, TextureHandle, Ui, ViewportCommand, pos2, vec2,
};
use egui_phosphor::regular as icon;
use hoardini_core::{JobRequest, paths};

use crate::{
    manager::Manager,
    model::{self, DownloadItem, Kind, Status},
    os,
    setup::{self, SetupState},
    tray::{self, Tray},
};

/// Decimal units, like the macOS file size formatter.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1000.0;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    match u {
        0 => format!("{v:.0} {}", UNITS[u]),
        1 => format!("{v:.1} {}", UNITS[u]),
        _ => format!("{v:.2} {}", UNITS[u]),
    }
}

fn duration(s: f64) -> String {
    let t = s as u64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

fn detail(item: &DownloadItem) -> String {
    match item.status {
        Status::Queued => "Waiting…".into(),
        Status::Failed => item.error.clone().unwrap_or_else(|| "Failed".into()),
        Status::Completed => {
            if item.converting {
                return "Converting to MP4…".into();
            }
            if item.needs_ffmpeg {
                return "Converts to MP4 once ffmpeg is installed".into();
            }
            if let Some(e) = &item.convert_error {
                return format!("Conversion failed: {e}");
            }
            let size = bytes(item.total.unwrap_or(item.done));
            match &item.mp4 {
                Some(m) => format!("{size} · MP4 ready: {}", m.file_name().unwrap_or_default().to_string_lossy()),
                None => size,
            }
        }
        Status::Paused | Status::Downloading => {
            let mut parts = Vec::new();
            match item.total {
                Some(t) => parts.push(format!("{} of {}", bytes(item.done), bytes(t))),
                None if item.done > 0 => parts.push(bytes(item.done)),
                None => {}
            }
            if item.status == Status::Paused {
                parts.push("Paused".into());
            }
            if item.speed > 0.0 {
                parts.push(format!("{}/s", bytes(item.speed as u64)));
            }
            if let Some(eta) = item.eta_seconds() {
                parts.push(format!("{} left", duration(eta)));
            }
            parts.join(" · ")
        }
    }
}

/// Shortens `text` in the middle ("long-file…name.mp4") so it fits in `width`.
fn middle_ellipsis(ui: &Ui, text: &str, font: &FontId, width: f32) -> String {
    let fits = |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x) <= width;
    if fits(text) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    let make = |keep: usize| {
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let s: String = chars[..head].iter().chain(['…'].iter()).chain(chars[chars.len() - tail..].iter()).collect();
        s
    };
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(&make(mid)) { lo = mid } else { hi = mid - 1 }
    }
    make(lo)
}

/// A thin progress bar; `None` draws a moving stripe (indeterminate).
fn progress_bar(ui: &mut Ui, fraction: Option<f32>, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, ui.visuals().widgets.inactive.bg_fill);
    match fraction {
        Some(f) => {
            let w = rect.width() * f.clamp(0.0, 1.0);
            if w > 0.0 {
                p.rect_filled(egui::Rect::from_min_size(rect.min, vec2(w.max(6.0), rect.height())), 3.0, color);
            }
        }
        None => {
            let t = ui.input(|i| i.time) as f32;
            let seg = rect.width() * 0.3;
            let x = rect.left() + ((t * 0.8).fract() * (rect.width() + seg)) - seg;
            let r = egui::Rect::from_min_max(pos2(x.max(rect.left()), rect.top()), pos2((x + seg).min(rect.right()), rect.bottom()));
            if r.width() > 0.0 {
                p.rect_filled(r, 3.0, color);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Pause,
    Resume,
    Convert,
    Open,
    Reveal,
    Cancel,
    RemoveFromList,
}

pub struct HoardiniApp {
    manager: Arc<Manager>,
    tray: Option<Tray>,
    setup: SetupState,
    show_setup: bool,
    adding: Option<String>,
    focus_add: bool,
    icon: Option<TextureHandle>,
    /// Started by the browser host: stay in the tray.
    hide_on_first_tick: bool,
    last_tray: Instant,
}

impl HoardiniApp {
    pub fn new(cc: &eframe::CreationContext<'_>, manager: Arc<Manager>, background: bool, icon_rgba: (Vec<u8>, u32)) -> Self {
        let ctx = cc.egui_ctx.clone();
        manager.set_context(ctx.clone());
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        ctx.set_fonts(fonts);

        let (rgba, size) = icon_rgba;
        let icon = Some(ctx.load_texture(
            "hoardini-icon",
            egui::ColorImage::from_rgba_unmultiplied([size as usize, size as usize], &rgba),
            egui::TextureOptions::LINEAR,
        ));
        let tray_px = 64u32;
        let tray_rgba = image::RgbaImage::from_raw(size, size, rgba)
            .map(|img| image::imageops::resize(&img, tray_px, tray_px, image::imageops::FilterType::Lanczos3).into_raw());
        let tray = tray_rgba.and_then(|r| tray::install(manager.clone(), ctx.clone(), r, tray_px));

        let setup = setup::install();
        // First launch: walk the user through the browser step once.
        let first_run = !manager.settings().extension_setup_shown;
        if first_run {
            manager.update_settings(|s| s.extension_setup_shown = true);
        }
        Self {
            manager,
            // Without a tray there would be no way back to a hidden window.
            hide_on_first_tick: background && tray.is_some() && !first_run,
            tray,
            setup,
            show_setup: first_run,
            adding: None,
            focus_add: false,
            icon,
            last_tray: Instant::now() - Duration::from_secs(5),
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn quit(&self) -> ! {
        self.manager.shutdown();
        std::process::exit(0);
    }

    fn open_add_dialog(&mut self) {
        self.adding = Some(String::new());
        self.focus_add = true;
    }

    fn pick_files_to_convert(&self) {
        let picked = rfd::FileDialog::new()
            .set_title("Choose MPEG-TS videos to convert to MP4")
            .add_filter("MPEG-TS video", &model::CONVERTIBLE)
            .set_directory(paths::downloads())
            .pick_files();
        if let Some(files) = picked {
            self.manager.convert_files(files);
        }
    }

    // ---------- toolbar ----------

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button(format!("{} Add URL", icon::PLUS)).on_hover_text("Ctrl+N").clicked() {
                self.open_add_dialog();
            }
            if ui.button(format!("{} Convert Files…", icon::FILM_STRIP)).on_hover_text("Convert existing .ts files to MP4").clicked() {
                self.pick_files_to_convert();
            }
            ui.menu_button(format!("{} Options", icon::GEAR), |ui| {
                let mut s = self.manager.settings();
                if ui.checkbox(&mut s.auto_convert, "Convert to MP4 automatically").changed()
                    || ui.checkbox(&mut s.delete_original, "Delete the .ts after converting").changed()
                {
                    self.manager.update_settings(|t| *t = s.clone());
                }
                ui.separator();
                if ui.button("Set Up Firefox Extension…").clicked() {
                    self.setup = setup::install();
                    self.show_setup = true;
                    ui.close();
                }
                if ui.button("Open Downloads Folder").clicked() {
                    os::open(&paths::downloads());
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit Hoardini").on_hover_text("Ctrl+Q").clicked() {
                    self.quit();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let finished = self.manager.finished_count();
                let label = if finished > 0 { format!("{} Clear Finished ({finished})", icon::CHECK_CIRCLE) } else { format!("{} Clear Finished", icon::CHECK_CIRCLE) };
                if ui
                    .add_enabled(finished > 0, egui::Button::new(label))
                    .on_hover_text("Remove finished downloads from this list (files stay in your Downloads folder) · Ctrl+Shift+Delete")
                    .on_disabled_hover_text("Nothing finished yet")
                    .clicked()
                {
                    self.manager.clear_finished();
                }
            });
        });
    }

    fn ffmpeg_notice(&self, ui: &mut Ui) {
        if self.manager.has_ffmpeg() {
            return;
        }
        let warn = ui.visuals().warn_fg_color;
        egui::Frame::new()
            .fill(warn.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, warn.gamma_multiply(0.5)))
            .corner_radius(6.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icon::WARNING).color(warn).size(16.0));
                    ui.label("ffmpeg isn't installed. Hoardini needs it to convert videos to MP4 and to save videos whose sound comes separately (Vimeo, Facebook HD).");
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("sudo apt install ffmpeg").monospace());
                    if ui.small_button(format!("{} Copy", icon::COPY)).clicked() {
                        ui.ctx().copy_text("sudo apt install ffmpeg".into());
                    }
                });
            });
        ui.add_space(6.0);
    }

    // ---------- list ----------

    fn empty_state(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 2.0 - 110.0).max(12.0));
            if let Some(t) = &self.icon {
                ui.add(egui::Image::new(t).fit_to_exact_size(vec2(96.0, 96.0)));
            }
            ui.add_space(8.0);
            ui.label(RichText::new("No downloads yet").heading());
            ui.label(RichText::new("Downloads sent from the Firefox extension appear here.").weak());
            ui.add_space(8.0);
            if ui.link("Set up the Firefox extension").clicked() {
                self.setup = setup::install();
                self.show_setup = true;
            }
        });
    }

    fn row(ui: &mut Ui, item: &DownloadItem) -> Option<Action> {
        let mut action = None;
        let visuals = ui.visuals().clone();
        let (glyph, tint) = match item.status {
            Status::Queued => (icon::CLOCK, visuals.weak_text_color()),
            Status::Downloading => (icon::ARROW_CIRCLE_DOWN, visuals.selection.bg_fill),
            Status::Paused => (icon::PAUSE_CIRCLE, visuals.weak_text_color()),
            Status::Completed => (icon::CHECK_CIRCLE, Color32::from_rgb(0x2e, 0xa0, 0x43)),
            Status::Failed => (icon::WARNING_CIRCLE, visuals.error_fg_color),
        };

        let mut buttons: Vec<(&str, &str, Action)> = Vec::new();
        match item.status {
            Status::Downloading if item.can_pause() => buttons.push((icon::PAUSE, "Pause", Action::Pause)),
            Status::Paused => buttons.push((icon::PLAY, "Resume", Action::Resume)),
            Status::Failed => buttons.push((icon::ARROW_CLOCKWISE, "Retry", Action::Resume)),
            Status::Completed => {
                if item.can_convert() {
                    buttons.push((icon::FILM_STRIP, "Convert to MP4", Action::Convert));
                }
                buttons.push(match item.kind() {
                    Kind::Video => (icon::MONITOR_PLAY, "Play", Action::Open),
                    Kind::Audio => (icon::PLAY_CIRCLE, "Play", Action::Open),
                    Kind::Other => (icon::ARROW_SQUARE_OUT, "Open", Action::Open),
                });
                buttons.push((icon::FOLDER_OPEN, "Show in Folder", Action::Reveal));
            }
            _ => {}
        }
        let cancel_tip = if item.status == Status::Completed { "Remove from list" } else { "Cancel and delete partial file" };
        buttons.push((icon::X, cancel_tip, Action::Cancel));

        let frame = egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 8)).corner_radius(6.0);
        // The row senses clicks (double-click to open, right-click menu) underneath its contents,
        // so the buttons on it still get their own clicks.
        let resp = ui
            .scope_builder(egui::UiBuilder::new().sense(Sense::click()), |ui| frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add_sized([28.0, 36.0], egui::Label::new(RichText::new(glyph).size(22.0).color(tint)));
                    let controls_w = buttons.len() as f32 * 30.0 + 4.0;
                    let text_w = (ui.available_width() - controls_w - 8.0).max(80.0);
                    ui.allocate_ui_with_layout(vec2(text_w, 40.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(text_w);
                        ui.spacing_mut().item_spacing.y = 4.0;
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let name = item.display_name();
                        let shown = middle_ellipsis(ui, &name, &font, text_w);
                        let l = ui.add(egui::Label::new(RichText::new(&shown).strong()).selectable(false));
                        if shown != name {
                            l.on_hover_text(&name);
                        }
                        if item.converting {
                            progress_bar(ui, None, visuals.selection.bg_fill);
                        } else if matches!(item.status, Status::Downloading | Status::Paused) {
                            let color = if item.status == Status::Paused { visuals.weak_text_color() } else { visuals.selection.bg_fill };
                            progress_bar(ui, item.fraction.map(|f| f as f32), color);
                        }
                        let color = if item.status == Status::Failed || item.convert_error.is_some() { visuals.error_fg_color } else { visuals.weak_text_color() };
                        ui.add(egui::Label::new(RichText::new(detail(item)).size(12.0).color(color)).truncate().selectable(false));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        for (glyph, tip, a) in buttons.into_iter().rev() {
                            let b = egui::Button::new(RichText::new(glyph).size(16.0)).frame(false).min_size(vec2(26.0, 26.0));
                            if ui.add(b).on_hover_text(tip).clicked() {
                                action = Some(a);
                            }
                        }
                    });
                });
            }))
            .response;
        if ui.rect_contains_pointer(resp.rect) {
            ui.painter().rect_stroke(resp.rect, 6.0, Stroke::new(1.0, visuals.widgets.hovered.bg_stroke.color.gamma_multiply(0.5)), egui::StrokeKind::Inside);
        }
        if resp.double_clicked() && item.status == Status::Completed {
            action = Some(Action::Open);
        }
        resp.context_menu(|ui| {
            if item.status == Status::Completed {
                if ui.button("Open").clicked() {
                    action = Some(Action::Open);
                }
                if ui.button("Show in Folder").clicked() {
                    action = Some(Action::Reveal);
                }
            }
            if ui.button("Copy Address").clicked() {
                ui.ctx().copy_text(item.request.url.clone());
            }
            if item.is_finished() {
                ui.separator();
                if ui.button("Remove from List").clicked() {
                    action = Some(Action::RemoveFromList);
                }
            }
        });
        action
    }

    fn list(&mut self, ui: &mut Ui, items: &[DownloadItem]) {
        let mut acts = Vec::new();
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    ui.separator();
                }
                if let Some(a) = Self::row(ui, item) {
                    acts.push((item.id, a));
                }
            }
        });
        let m = &self.manager;
        for (id, a) in acts {
            match a {
                Action::Pause => m.pause(id),
                Action::Resume => m.resume(id),
                Action::Convert => m.convert(id),
                Action::Open => {
                    if let Some(p) = m.playable(id) {
                        os::open(&p);
                    }
                }
                Action::Reveal => {
                    if let Some(p) = m.playable(id) {
                        os::reveal(&p);
                    }
                }
                Action::Cancel => m.cancel_and_remove(id),
                Action::RemoveFromList => m.remove_from_list(id),
            }
        }
    }

    // ---------- dialogs ----------

    fn add_dialog(&mut self, ctx: &egui::Context) {
        let Some(text) = self.adding.as_mut() else { return };
        let mut close = false;
        let mut submit = false;
        Modal::new(Id::new("add-url")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new("Add download").heading());
            ui.add_space(6.0);
            let edit = ui.add(egui::TextEdit::singleline(text).hint_text("https://…").desired_width(f32::INFINITY));
            if std::mem::take(&mut self.focus_add) {
                edit.request_focus();
            }
            let valid = JobRequest::new(text.trim()).valid_url().is_some();
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && valid {
                submit = true;
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(valid, egui::Button::new("Download")).clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if submit {
            let url = text.trim().to_owned();
            self.manager.add(JobRequest::new(url));
            close = true;
        }
        if close {
            self.adding = None;
        }
    }

    fn setup_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_setup {
            return;
        }
        let mut close = false;
        let ok = Color32::from_rgb(0x2e, 0xa0, 0x43);
        Modal::new(Id::new("setup")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(RichText::new("Add the Hoardini extension to Firefox").heading());
            ui.add_space(8.0);
            match &self.setup.host {
                Ok(_) => {
                    ui.label(RichText::new(format!("{} Firefox can reach Hoardini (native messaging is set up).", icon::CHECK)).color(ok));
                }
                Err(e) => {
                    ui.label(RichText::new(format!("{} Firefox can't reach Hoardini yet: {e}", icon::WARNING)).color(ui.visuals().error_fg_color));
                }
            }
            ui.add_space(8.0);
            if let Some(xpi) = self.setup.xpi.clone() {
                ui.label("Click Install, then confirm with Add in Firefox. You only need to do this once.");
                ui.add_space(6.0);
                if ui.button(format!("{} Install in Firefox", icon::PUZZLE_PIECE)).clicked() {
                    setup::open_in_firefox(&xpi.to_string_lossy());
                }
            } else if let Some(dir) = self.setup.extension_dir.clone() {
                ui.label(
                    "This copy of Hoardini doesn't include a Mozilla-signed extension, and Firefox only keeps signed ones. \
                     Until you have the signed hoardini.xpi, load it as a temporary add-on (Firefox removes it when it restarts):",
                );
                ui.add_space(6.0);
                ui.label("1. Open about:debugging, This Firefox.");
                ui.label("2. Click Load Temporary Add-on…");
                ui.label("3. Choose manifest.json in this folder:");
                ui.add(egui::Label::new(RichText::new(dir.display().to_string()).monospace()).selectable(true));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Open about:debugging").clicked() {
                        setup::open_in_firefox("about:debugging#/runtime/this-firefox");
                    }
                    if ui.button(format!("{} Copy Folder Path", icon::COPY)).clicked() {
                        ui.ctx().copy_text(dir.display().to_string());
                    }
                    if ui.button(format!("{} Show Folder", icon::FOLDER_OPEN)).clicked() {
                        os::open(&dir);
                    }
                });
            } else {
                ui.label("The extension files weren't found next to Hoardini. Reinstall Hoardini to get them back.");
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Done").clicked() {
                    close = true;
                }
            });
        });
        if close || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.show_setup = false;
        }
    }
}

impl eframe::App for HoardiniApp {
    /// Runs every tick, also while the window is hidden in the tray.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.hide_on_first_tick) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
        if let Some(t) = &self.tray {
            let a = t.actions.swap(0, Ordering::SeqCst);
            if a & tray::QUIT != 0 {
                self.quit();
            }
            if a & tray::SETUP != 0 {
                self.setup = setup::install();
                self.show_setup = true;
            }
            if a & tray::SHOW != 0 {
                self.show_window(ctx);
            }
        }
        if self.manager.quit_requested.load(Ordering::SeqCst) {
            self.quit();
        }
        if self.manager.show_requested.swap(false, Ordering::SeqCst) {
            self.show_window(ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.tray.is_some() {
                // Closing the window keeps downloading; the tray icon stays.
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            } else {
                self.manager.shutdown();
            }
        }
        if self.last_tray.elapsed() >= Duration::from_secs(1) {
            self.last_tray = Instant::now();
            if let Some(t) = &mut self.tray {
                t.refresh(&self.manager);
            }
        }
        let (active, _) = self.manager.activity();
        ctx.request_repaint_after(if active > 0 { Duration::from_millis(250) } else { Duration::from_secs(1) });
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let modal_open = self.adding.is_some() || self.show_setup;
        if !modal_open {
            let new = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
            let quit = KeyboardShortcut::new(Modifiers::COMMAND, Key::Q);
            let close = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
            let clear = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Delete);
            if ctx.input_mut(|i| i.consume_shortcut(&new)) {
                self.open_add_dialog();
            }
            if ctx.input_mut(|i| i.consume_shortcut(&quit)) {
                self.quit();
            }
            if ctx.input_mut(|i| i.consume_shortcut(&close)) {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&clear)) {
                self.manager.clear_finished();
            }
        }

        egui::Panel::top("toolbar").frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 8))).show(ui, |ui| {
            self.toolbar(ui);
        });
        let items = self.manager.items();
        egui::CentralPanel::default().show(ui, |ui| {
            self.ffmpeg_notice(ui);
            if items.is_empty() {
                self.empty_state(ui);
            } else {
                self.list(ui, &items);
            }
        });
        self.add_dialog(&ctx);
        self.setup_dialog(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn finished_video() -> DownloadItem {
        let mut item = DownloadItem::new(JobRequest::new("https://x.example/v.mp4"));
        item.status = Status::Completed;
        item.destination = Some(PathBuf::from("/tmp/clip.mp4"));
        item.total = Some(1000);
        item
    }

    /// Each button on a finished download does its own thing; the row underneath doesn't take the click.
    #[test]
    fn row_buttons_get_their_clicks() {
        for (glyph, expected) in [
            (icon::MONITOR_PLAY, Action::Open),
            (icon::FOLDER_OPEN, Action::Reveal),
            (icon::X, Action::Cancel),
        ] {
            let item = finished_video();
            let mut harness = Harness::new_ui_state(
                move |ui, got: &mut Option<Action>| {
                    if let Some(a) = HoardiniApp::row(ui, &item) {
                        *got = Some(a);
                    }
                },
                None,
            );
            harness.get_by_label(glyph).click();
            harness.run();
            assert_eq!(harness.state(), &Some(expected));
        }
    }
}
