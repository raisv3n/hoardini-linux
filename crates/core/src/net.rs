use std::{
    ffi::{CString, OsString},
    fs::{File, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{Client, RequestBuilder, Response, StatusCode, header, redirect};
use url::Url;

use crate::{Error, Result, probe::split_ext};

/// Redirects are not followed by reqwest: its policy keeps Cookie/Authorization across a same-port
/// https -> http downgrade and can't edit headers, so [`get`] follows them itself (the macOS `RedirectGuard`).
/// The timeouts mirror URLSession's defaults: a stalled server fails (and is retried) instead of hanging.
pub fn client() -> Client {
    Client::builder()
        .user_agent(concat!("Hoardini/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .redirect(redirect::Policy::none())
        .build()
        .expect("HTTP client")
}

pub fn apply_headers(mut req: RequestBuilder, headers: &[(String, String)]) -> RequestBuilder {
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req
}

/// Whether credentials supplied for `origin` may go to `url`: same host, and never from https down to
/// plain http (that would put Secure cookies on the wire unencrypted). Ports don't matter: cookies
/// are not port-scoped.
pub fn may_carry_credentials(url: &Url, origin: &Url) -> bool {
    let same_host = url.host_str().map(str::to_ascii_lowercase) == origin.host_str().map(str::to_ascii_lowercase);
    same_host && !(origin.scheme() == "https" && url.scheme() != "https")
}

fn is_credential(name: &str) -> bool {
    name.eq_ignore_ascii_case("cookie") || name.eq_ignore_ascii_case("authorization")
}

/// Headers safe to send to `url`: credentials only go where [`may_carry_credentials`] allows.
pub fn headers_for(headers: &[(String, String)], url: &Url, origin: &Url) -> Vec<(String, String)> {
    let keep = may_carry_credentials(url, origin);
    headers.iter().filter(|(k, _)| keep || !is_credential(k)).cloned().collect()
}

pub const MAX_REDIRECTS: usize = 10;

/// `GET url` with `headers` (and an optional `Range`), following up to [`MAX_REDIRECTS`] redirects.
/// Credentials go to a hop only if [`may_carry_credentials`] allows it from both the job's `origin`
/// and the first URL of the chain, so a redirect can't carry them to another host or down to http.
/// `Response::url()` is the last hop.
pub async fn get(client: &Client, url: &Url, headers: &[(String, String)], origin: &Url, range: Option<String>) -> Result<Response> {
    let mut current = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let keep = may_carry_credentials(&current, origin) && may_carry_credentials(&current, url);
        let hop: Vec<_> = headers.iter().filter(|(k, _)| keep || !is_credential(k)).cloned().collect();
        let mut req = apply_headers(client.get(current.clone()), &hop);
        if let Some(r) = &range {
            req = req.header(header::RANGE, r);
        }
        let resp = req.send().await?;
        if !resp.status().is_redirection() || resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(resp);
        }
        let next = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| current.join(l).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"));
        match next {
            Some(n) => current = n,
            // No usable Location: the redirect is the answer.
            None => return Ok(resp),
        }
    }
    Err(Error::TooManyRedirects)
}

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// `x.mp4` + `.part` -> `x.mp4.part`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn without_credentials(u: &Url) -> String {
    let mut u = u.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.to_string()
}

fn set_xattr(file: &Path, name: &str, value: &str) {
    let (Ok(p), Ok(n)) = (CString::new(file.as_os_str().as_bytes()), CString::new(name)) else { return };
    unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
}

/// Tags a downloaded file with where it came from, the way Firefox and Chrome do on Linux
/// (the freedesktop `user.xdg.origin.url` / `user.xdg.referrer.url` attributes; file managers show them).
/// The counterpart of the macOS quarantine attribute. Silently skipped on file systems without xattrs.
pub fn mark_origin(file: &Path, source: &Url, referrer: Option<&str>) {
    set_xattr(file, "user.xdg.origin.url", &without_credentials(source));
    if let Some(r) = referrer.and_then(|r| Url::parse(r).ok()) {
        set_xattr(file, "user.xdg.referrer.url", &without_credentials(&r));
    }
}

/// Hidden per-track temp file of a split (video + audio) download: `.<final name>.<track>.<ext>`.
pub fn track_file(final_path: &Path, track: &str, ext: &str) -> PathBuf {
    let name = final_path.file_name().unwrap_or_default().to_string_lossy();
    final_path.with_file_name(format!(".{name}.{track}.{ext}"))
}

/// Deletes everything an unfinished download leaves next to `final_path`: the .part file, the resume
/// state, and the hidden track files (with their own .part/.hoardini). Call it only once the download
/// has stopped, or it may recreate them.
pub fn remove_partial(final_path: &Path) {
    for s in [".part", ".hoardini"] {
        let _ = std::fs::remove_file(with_suffix(final_path, s));
    }
    let (Some(dir), Some(name)) = (final_path.parent(), final_path.file_name()) else { return };
    let prefix = format!(".{}.", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        if f.strip_prefix(&prefix).is_some_and(|rest| rest.starts_with("video.") || rest.starts_with("audio.")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// An advisory lock (`flock`) on `<name>.lock`, held while a download writes `<name>`.
/// Dropping it deletes the lock file and releases the lock.
pub struct DestinationLock {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for DestinationLock {
    fn drop(&mut self) {
        // Unlink while still holding the lock, so nobody can take over a file that is about to vanish.
        let _ = std::fs::remove_file(&self.path);
        self.file.take();
    }
}

enum Lock {
    Got(File),
    Busy,
}

fn try_lock(path: &Path) -> io::Result<Lock> {
    loop {
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o644).open(path)?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EWOULDBLOCK) { Ok(Lock::Busy) } else { Err(e) };
        }
        // The holder may have unlinked the file between our open and flock; then we locked a ghost.
        let same = match (f.metadata(), std::fs::metadata(path)) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => false,
        };
        if same {
            return Ok(Lock::Got(f));
        }
    }
}

/// Picks a destination name nobody else is using and holds a lock on it.
/// A name is usable if we get its lock and it is either a resumable download or not yet taken.
pub fn claim(directory: &Path, filename: &str) -> Result<(PathBuf, DestinationLock)> {
    let (base, ext) = split_ext(filename);
    for n in 0..1000 {
        let name = match (n, ext) {
            (0, _) => filename.to_owned(),
            (_, Some(e)) => format!("{base} ({n}).{e}"),
            (_, None) => format!("{base} ({n})"),
        };
        let final_path = directory.join(&name);
        let lock_path = with_suffix(&final_path, ".lock");
        match try_lock(&lock_path) {
            Ok(Lock::Got(file)) => {
                let lock = DestinationLock { path: lock_path, file: Some(file) };
                let resumable = with_suffix(&final_path, ".hoardini").exists();
                if resumable || !final_path.exists() {
                    return Ok((final_path, lock));
                }
                // Taken by a finished file: drop the lock (and its file) and try the next name.
            }
            Ok(Lock::Busy) => {}
            Err(_) => break,
        }
    }
    Err(Error::CannotWrite(directory.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_skips_taken_and_locked_names() {
        let dir = tempfile::tempdir().unwrap();
        let (a, lock_a) = claim(dir.path(), "x.bin").unwrap();
        assert_eq!(a.file_name().unwrap(), "x.bin");
        // Still locked by `lock_a`.
        let (b, _lock_b) = claim(dir.path(), "x.bin").unwrap();
        assert_eq!(b.file_name().unwrap(), "x (1).bin");
        drop(lock_a);
        assert!(!with_suffix(&a, ".lock").exists(), "lock file is removed on release");
        std::fs::write(&a, b"done").unwrap();
        let (c, _lock_c) = claim(dir.path(), "x.bin").unwrap();
        assert_eq!(c.file_name().unwrap(), "x (2).bin");
        assert!(!dir.path().join("x.bin.lock").exists(), "a skipped name leaves no lock file");
    }

    #[test]
    fn claim_reuses_resumable_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.bin"), b"old").unwrap();
        std::fs::write(dir.path().join("x.bin.hoardini"), b"{}").unwrap();
        let (a, _l) = claim(dir.path(), "x.bin").unwrap();
        assert_eq!(a.file_name().unwrap(), "x.bin");
    }

    #[test]
    fn origin_attributes_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.zip");
        std::fs::write(&f, b"x").unwrap();
        mark_origin(&f, &Url::parse("https://u:p@example.com/x").unwrap(), Some("https://example.com/page"));
        let read = |name: &str| {
            let p = CString::new(f.as_os_str().as_bytes()).unwrap();
            let n = CString::new(name).unwrap();
            let mut buf = [0u8; 256];
            let len = unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
            (len >= 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
        };
        // tmpfs without user xattrs: nothing to check.
        let Some(origin) = read("user.xdg.origin.url") else { return };
        assert_eq!(origin, "https://example.com/x");
        assert_eq!(read("user.xdg.referrer.url").as_deref(), Some("https://example.com/page"));
    }

    #[test]
    fn remove_partial_deletes_only_this_downloads_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("x.mp4");
        let ours = ["x.mp4.part", "x.mp4.hoardini", ".x.mp4.video.mp4", ".x.mp4.audio.m4a", ".x.mp4.video.mp4.part", ".x.mp4.audio.mp4.hoardini"];
        let others = ["x.mp4", "y.mp4.part", ".y.mp4.video.mp4", ".x.mp4.other", "x.mp4.video.mp4"];
        for f in ours.iter().chain(&others) {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        assert_eq!(track_file(&final_path, "video", "mp4"), dir.path().join(".x.mp4.video.mp4"));
        remove_partial(&final_path);
        for f in ours {
            assert!(!dir.path().join(f).exists(), "{f} should be gone");
        }
        for f in others {
            assert!(dir.path().join(f).exists(), "{f} should stay");
        }
    }

    #[test]
    fn credentials_stay_on_origin_host() {
        let h = vec![("Cookie".to_owned(), "a=b".to_owned()), ("Referer".to_owned(), "r".to_owned())];
        let origin = Url::parse("https://site.example/a").unwrap();
        let cdn = Url::parse("https://cdn.example/a").unwrap();
        assert_eq!(headers_for(&h, &cdn, &origin), vec![("Referer".to_owned(), "r".to_owned())]);
        assert_eq!(headers_for(&h, &Url::parse("https://SITE.example/b").unwrap(), &origin), h);
    }

    fn creds() -> Vec<(String, String)> {
        [("Cookie", "a=b"), ("Authorization", "Bearer x"), ("Referer", "https://site.com/")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .to_vec()
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn https_downgrade_drops_credentials() {
        let h = headers_for(&creds(), &u("http://site.com/seg.ts"), &u("https://site.com/master.m3u8"));
        assert_eq!(h, vec![("Referer".to_owned(), "https://site.com/".to_owned())]);
        // Same port both ways: reqwest's own redirect policy would have kept them here.
        assert!(!may_carry_credentials(&u("http://site.com:8080/a"), &u("https://site.com:8080/b")));
    }

    #[test]
    fn other_host_drops_credentials() {
        let h = headers_for(&creds(), &u("https://evil.example/seg.ts"), &u("https://site.com/master.m3u8"));
        assert_eq!(h, vec![("Referer".to_owned(), "https://site.com/".to_owned())]);
    }

    #[test]
    fn upgrade_and_other_port_keep_credentials() {
        assert_eq!(headers_for(&creds(), &u("https://cdn.site.com/a"), &u("https://CDN.site.com/b")), creds());
        assert!(may_carry_credentials(&u("https://site.com/a"), &u("http://site.com/b")));
        assert!(may_carry_credentials(&u("https://site.com:8443/a"), &u("https://site.com/b")));
        assert!(may_carry_credentials(&u("http://site.com/a"), &u("http://site.com/b")));
    }
}
