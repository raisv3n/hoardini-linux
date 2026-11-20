//! Lossless container conversion. The macOS app used AVFoundation's passthrough export;
//! on Linux the equivalent is ffmpeg with `-c copy` (no re-encoding).

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use crate::{
    Error, Result, net,
    net::with_suffix,
};

/// `HOARDINI_FFMPEG` if set (exactly that file, nothing else), else `ffmpeg` on PATH or in the usual
/// places (apps started from a desktop menu can have a short PATH).
pub fn ffmpeg_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HOARDINI_FFMPEG") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    path_dirs
        .into_iter()
        .chain(["/usr/bin", "/usr/local/bin", "/snap/bin"].map(PathBuf::from))
        .map(|d| d.join("ffmpeg"))
        .find(|p| p.is_file())
}

/// Deletes a half-written output unless the operation completed, including when the task is aborted.
struct RemoveOnDrop<'a>(&'a Path, bool);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        if !self.1 {
            let _ = fs::remove_file(self.0);
        }
    }
}

async fn ffmpeg(args: Vec<OsString>) -> Result<()> {
    let exe = ffmpeg_path().ok_or(Error::FfmpegMissing)?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Pausing aborts the task, which drops (and kills) the child.
        .kill_on_drop(true);
    // If Hoardini itself dies, ffmpeg goes too.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let out = cmd.spawn()?.wait_with_output().await?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let msg = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("ffmpeg failed").to_owned();
    if ["does not contain any stream", "Invalid data found", "matches no streams"].iter().any(|s| stderr.contains(s)) {
        return Err(Error::Unreadable);
    }
    Err(Error::Remux(msg))
}

fn os(args: &[&dyn AsRef<std::ffi::OsStr>]) -> Vec<OsString> {
    args.iter().map(|a| a.as_ref().to_owned()).collect()
}

/// The ffmpeg demuxer for a file Hoardini wrote or converts, by extension. Inputs never go to ffmpeg's
/// content sniffing: a server could send a playlist or other container named `.ts`, and ffmpeg would
/// then follow what it points to.
pub fn demuxer(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "ts" | "mts" | "m2ts" => "mpegts",
        "mp4" | "m4a" | "m4v" | "mov" => "mov",
        "aac" => "aac",
        "mp3" => "mp3",
        "ac3" => "ac3",
        "ec3" => "eac3",
        _ => return None,
    })
}

/// `-f <demuxer> -i file:<path>`: a fixed demuxer, and the file protocol whatever the name looks like.
fn input(path: &Path) -> Result<Vec<OsString>> {
    let format = demuxer(path).ok_or(Error::Unreadable)?;
    let mut file = OsString::from("file:");
    file.push(path.as_os_str());
    Ok(vec!["-f".into(), format.into(), "-i".into(), file])
}

fn output(path: &Path) -> OsString {
    let mut file = OsString::from("file:");
    file.push(path.as_os_str());
    file
}

/// Copies the audio/video streams of `source` into an MP4 next to it, without re-encoding.
pub async fn to_mp4(source: &Path) -> Result<PathBuf> {
    // Only MPEG-TS is converted (see model::CONVERTIBLE in the app).
    if !source.is_file() || demuxer(source) != Some("mpegts") {
        return Err(Error::Unreadable);
    }
    if ffmpeg_path().is_none() {
        return Err(Error::FfmpegMissing);
    }
    let dir = source.parent().unwrap_or(Path::new("."));
    let base = source.file_stem().unwrap_or_default().to_string_lossy();
    let (final_path, _lock) = net::claim(dir, &format!("{base}.mp4"))?;
    let tmp = with_suffix(&final_path, ".part");
    let _ = fs::remove_file(&tmp);
    let mut guard = RemoveOnDrop(&tmp, false);
    #[rustfmt::skip]
    let args = [input(source)?, os(&[
        &"-map", &"0:v?", &"-map", &"0:a?", &"-c", &"copy",
        // Index at the front: instant seeking.
        &"-movflags", &"+faststart", &"-f", &"mp4", &output(&tmp),
    ])].concat();
    ffmpeg(args).await?;
    fs::rename(&tmp, &final_path)?;
    guard.1 = true;
    Ok(final_path)
}

/// Combines a video-only and an audio-only file into one MP4, without re-encoding.
/// Pausing mid-merge kills ffmpeg; the track files stay, so resuming just merges again.
pub async fn merge(video: &Path, audio: &Path, output: &Path) -> Result<()> {
    let mut guard = RemoveOnDrop(output, false);
    #[rustfmt::skip]
    let args = [input(video)?, input(audio)?, os(&[
        &"-map", &"0:v:0", &"-map", &"1:a:0", &"-c", &"copy",
        &"-movflags", &"+faststart", &"-f", &"mp4", &self::output(output),
    ])].concat();
    ffmpeg(args).await?;
    guard.1 = true;
    Ok(())
}
