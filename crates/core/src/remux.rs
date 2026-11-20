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

/// Copies the audio/video streams of `source` into an MP4 next to it, without re-encoding.
pub async fn to_mp4(source: &Path) -> Result<PathBuf> {
    if !source.is_file() {
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
    let args = os(&[
        &"-i", &source, &"-map", &"0:v?", &"-map", &"0:a?", &"-c", &"copy",
        // Index at the front: instant seeking.
        &"-movflags", &"+faststart", &"-f", &"mp4", &tmp,
    ]);
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
    let args = os(&[
        &"-i", &video, &"-i", &audio, &"-map", &"0:v:0", &"-map", &"1:a:0", &"-c", &"copy",
        &"-movflags", &"+faststart", &"-f", &"mp4", &output,
    ]);
    ffmpeg(args).await?;
    guard.1 = true;
    Ok(())
}
