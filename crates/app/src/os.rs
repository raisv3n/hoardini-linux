//! Desktop integration (the NSWorkspace calls of the macOS version): open files, show them in the
//! file manager, and the clipboard-free bits of talking to the desktop.

use std::{
    path::Path,
    process::{Command, Stdio},
};

fn spawn_reaped(cmd: &mut Command) -> bool {
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Opens a file in its default app.
pub fn open(path: &Path) {
    spawn_reaped(Command::new("xdg-open").arg(path));
}

pub fn open_url(url: &str) {
    spawn_reaped(Command::new("xdg-open").arg(url));
}

/// Shows the file selected in the file manager (org.freedesktop.FileManager1), or opens its folder.
pub fn reveal(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let shown = url::Url::from_file_path(&path).ok().is_some_and(|uri| {
            Command::new("dbus-send")
                .args([
                    "--session",
                    "--print-reply",
                    "--reply-timeout=3000",
                    "--dest=org.freedesktop.FileManager1",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1.ShowItems",
                ])
                // dbus-send splits array items at commas.
                .arg(format!("array:string:{}", uri.as_str().replace(',', "%2C")))
                .arg("string:")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        });
        if !shown && let Some(dir) = path.parent() {
            open(dir);
        }
    });
}
