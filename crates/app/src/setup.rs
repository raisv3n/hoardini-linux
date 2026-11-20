//! Connects Firefox to the app (Setup.swift). On every launch:
//! - registers the native-messaging host (`~/.mozilla/native-messaging-hosts/com.hoardini.host.json`),
//!   pointing at the `hoardini-host` installed next to this program, so moving or updating the app
//!   keeps the browser connected;
//! - copies the unpacked extension to `~/.local/share/hoardini/extension`, a folder that never moves,
//!   for loading it as a temporary add-on while developing.
//!
//! Firefox only keeps add-ons signed by Mozilla, so a release ships `hoardini.xpi` (signed through
//! addons.mozilla.org) and the setup dialog opens it in Firefox.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use hoardini_core::paths;

pub const HOST_NAME: &str = "com.hoardini.host";
pub const EXTENSION_ID: &str = "hoardini@raisv3n";

/// Firefox and its forks that read the same native-messaging manifest format, each from its own folder.
/// Firefox's folder is always written; the others only when that browser has a profile folder.
const BROWSERS: [(&str, bool); 3] = [(".mozilla", true), (".librewolf", false), (".waterfox", false)];

#[derive(Clone, Debug)]
pub struct SetupState {
    /// Where the host manifest was written, or why not.
    pub host: Result<PathBuf, String>,
    /// A Mozilla-signed extension package shipped with this build.
    pub xpi: Option<PathBuf>,
    /// The unpacked extension, for a temporary install.
    pub extension_dir: Option<PathBuf>,
}

/// Where packaged data lives relative to the binary: `<prefix>/bin/hoardini-app` ->
/// `<prefix>/share/hoardini`; a release folder keeps it next to the binary; a source build uses the repo.
fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        if let Some(prefix) = dir.parent() {
            v.push(prefix.join("share/hoardini"));
        }
        v.push(dir.clone());
    }
    v.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    v
}

fn find_xpi() -> Option<PathBuf> {
    data_dirs().into_iter().map(|d| d.join("hoardini.xpi")).find(|p| p.is_file())
}

fn find_extension_source() -> Option<PathBuf> {
    data_dirs().into_iter().map(|d| d.join("extension")).find(|p| p.join("manifest.json").is_file())
}

pub fn extension_target() -> PathBuf {
    paths::support().join("extension")
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else if e.file_type()?.is_file() {
            fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

/// Copies next to the target, then swaps it in, so a half-finished copy is never left behind.
fn replace_dir(target: &Path, source: &Path) -> io::Result<()> {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let staged = target.with_file_name(format!(".{name}.new"));
    let old = target.with_file_name(format!(".{name}.old"));
    let _ = fs::remove_dir_all(&staged);
    copy_dir(source, &staged)?;
    let _ = fs::remove_dir_all(&old);
    if target.exists() {
        fs::rename(target, &old)?;
    }
    fs::rename(&staged, target)?;
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

pub fn host_manifest(host: &Path) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": "Hoardini native host",
        "path": host,
        "type": "stdio",
        "allowed_extensions": [EXTENSION_ID],
    })
}

fn register_host() -> Result<PathBuf, String> {
    let host = paths::sibling(paths::HOST_BIN).ok_or("hoardini-host is not installed next to Hoardini")?;
    let host = fs::canonicalize(&host).unwrap_or(host);
    let home = dirs::home_dir().ok_or("no home folder")?;
    let data = serde_json::to_vec_pretty(&host_manifest(&host)).map_err(|e| e.to_string())?;
    let mut firefox = None;
    for (dir, always) in BROWSERS {
        let root = home.join(dir);
        if !always && !root.is_dir() {
            continue;
        }
        let folder = root.join("native-messaging-hosts");
        let file = folder.join(format!("{HOST_NAME}.json"));
        let r = fs::create_dir_all(&folder).and_then(|_| paths::write_atomic(&file, &data, 0o644));
        if always {
            r.map_err(|e| format!("could not write {}: {e}", file.display()))?;
            firefox = Some(file);
        }
    }
    firefox.ok_or_else(|| "no browser folder".into())
}

/// Runs the whole setup. Cheap enough for every launch.
pub fn install() -> SetupState {
    let extension_dir = find_extension_source().and_then(|src| {
        let target = extension_target();
        if fs::canonicalize(&src).ok() == fs::canonicalize(&target).ok() {
            return Some(target);
        }
        replace_dir(&target, &src).ok().map(|_| target)
    });
    SetupState { host: register_host(), xpi: find_xpi(), extension_dir }
}

fn spawn_quiet(cmd: &mut Command) -> bool {
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            // Reap it so it doesn't linger as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Opens `target` (a file or a URL) in Firefox, falling back to the default browser.
pub fn open_in_firefox(target: &str) {
    if !spawn_quiet(Command::new("firefox").arg(target)) {
        crate::os::open_url(target);
    }
}
