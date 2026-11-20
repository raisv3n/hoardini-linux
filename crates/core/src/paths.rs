//! Where Hoardini keeps its files, and the single-instance lock (the Linux stand-in for the
//! bundle-ID lookups the macOS host used to find the running app).

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::PermissionsExt, io::AsRawFd},
    path::{Path, PathBuf},
};

pub const APP_BIN: &str = "hoardini-app";
pub const CLI_BIN: &str = "hoardini";
pub const HOST_BIN: &str = "hoardini-host";

/// `~/.local/share/hoardini`: the download list, settings and the browser host's inbox.
pub fn support() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| home().join(".local/share")).join("hoardini")
}

/// Jobs dropped here by the browser host are picked up by the running app.
pub fn inbox() -> PathBuf {
    support().join("inbox")
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The XDG Downloads folder (`xdg-user-dir DOWNLOAD`), else `~/Downloads`.
pub fn downloads() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| home().join("Downloads"))
}

/// Creates `dir` (and its parents) readable only by this user. The inbox carries cookies.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// A sibling of the running executable, e.g. `hoardini` next to `hoardini-host`.
pub fn sibling(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join(name);
    p.is_file().then_some(p)
}

fn lock_path() -> PathBuf {
    support().join("app.lock")
}

/// Held for the app's lifetime; the kernel drops the lock if the process dies.
pub struct SingleInstance(#[allow(dead_code)] File);

impl SingleInstance {
    /// `None` when another instance already holds the lock.
    pub fn acquire() -> Option<Self> {
        let _ = private_dir(&support());
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock_path()).ok()?;
        (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(Self(f))
    }
}

/// True while some process holds the app lock.
pub fn app_running() -> bool {
    let Ok(f) = File::open(lock_path()) else { return false };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    true
}

/// Writes `data` to `path` through a temp file and a rename, so readers never see half a file.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
    f.write_all(data)?;
    f.sync_data()?;
    drop(f);
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_sets_mode() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("q.json");
        write_atomic(&p, b"one", 0o600).unwrap();
        write_atomic(&p, b"two", 0o600).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp files left");
    }
}
