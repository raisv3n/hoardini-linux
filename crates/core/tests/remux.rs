//! Conversion and merging with the real ffmpeg. Skipped when ffmpeg isn't installed (CI installs it).

use std::{path::Path, process::Command};

use hoardini_core::{Error, remux};

/// Generates a short clip with ffmpeg's built-in test sources.
fn make(out: &Path, args: &[&str]) {
    let ffmpeg = remux::ffmpeg_path().unwrap();
    let ok = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(out)
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not generate {out:?}");
}

const VIDEO: [&str; 8] = ["-f", "lavfi", "-i", "testsrc=duration=1:size=64x48:rate=10", "-c:v", "mpeg2video", "-an", "-y"];
const AUDIO: [&str; 6] = ["-f", "lavfi", "-i", "sine=duration=1", "-c:a", "aac"];

#[tokio::test]
async fn converts_mpeg_ts() {
    if remux::ffmpeg_path().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("clip.ts");
    make(&ts, &[&VIDEO[..], &AUDIO[..], &["-f", "mpegts"]].concat());
    let out = remux::to_mp4(&ts).await.unwrap();
    assert_eq!(out, dir.path().join("clip.mp4"));
    assert!(std::fs::metadata(&out).unwrap().len() > 0);
}

#[tokio::test]
async fn merges_video_and_audio_tracks() {
    if remux::ffmpeg_path().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // Named like the hidden track files the HLS and pair downloads write.
    let video = dir.path().join(".x.mp4.video.mp4");
    let audio = dir.path().join(".x.mp4.audio.m4a");
    make(&video, &[&VIDEO[..], &["-f", "mp4"]].concat());
    make(&audio, &[&AUDIO[..], &["-f", "ipod"]].concat());
    let out = dir.path().join("x.mp4.part");
    remux::merge(&video, &audio, &out).await.unwrap();
    assert!(std::fs::metadata(&out).unwrap().len() > 0);
}

/// A "video" that is really an HLS playlist pointing at another local file: ffmpeg must read it as
/// MPEG-TS (and fail), not follow the playlist into the other file. Recent ffmpeg (6+) already refuses
/// to sniff HLS from a `.ts` name; the forced demuxer is what protects older ones (e.g. 4.4).
#[tokio::test]
async fn disguised_playlist_is_not_followed() {
    if remux::ffmpeg_path().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.ts");
    make(&secret, &[&VIDEO[..], &["-f", "mpegts"]].concat());
    let evil = dir.path().join("evil.ts");
    let playlist = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{}\n#EXT-X-ENDLIST\n", secret.display());
    std::fs::write(&evil, playlist).unwrap();
    let r = remux::to_mp4(&evil).await;
    assert!(matches!(r, Err(Error::Unreadable | Error::Remux(_))), "{r:?}");
    assert!(!dir.path().join("evil.mp4").exists());
}

#[tokio::test]
async fn only_known_inputs() {
    assert_eq!(remux::demuxer(Path::new("/d/a.M2TS")), Some("mpegts"));
    assert_eq!(remux::demuxer(Path::new("/d/.x.mp4.audio.m4a")), Some("mov"));
    assert_eq!(remux::demuxer(Path::new("/d/a.ec3")), Some("eac3"));
    assert_eq!(remux::demuxer(Path::new("/d/a.m3u8")), None);
    assert_eq!(remux::demuxer(Path::new("/d/noext")), None);
    let dir = tempfile::tempdir().unwrap();
    let mp4 = dir.path().join("a.mp4");
    std::fs::write(&mp4, b"x").unwrap();
    assert!(matches!(remux::to_mp4(&mp4).await, Err(Error::Unreadable)), "only MPEG-TS is converted");
}
