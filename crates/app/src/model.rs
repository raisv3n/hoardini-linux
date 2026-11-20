use std::path::{Path, PathBuf};

use hoardini_core::JobRequest;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
}

/// What a finished file is, for the Open button's icon and wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Other,
}

/// Only video containers ffmpeg can remux losslessly into MP4 (MPEG-TS from HLS downloads).
pub const CONVERTIBLE: [&str; 3] = ["ts", "mts", "m2ts"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadItem {
    pub id: Uuid,
    pub request: JobRequest,
    pub status: Status,
    /// Final file path, known once the downloader picks a name.
    #[serde(default)]
    pub destination: Option<PathBuf>,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub fraction: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
    /// Unix time.
    #[serde(default)]
    pub added: i64,
    /// Bytes/s, smoothed; not meaningful across launches.
    #[serde(skip)]
    pub speed: f64,
    /// Converted copy, when one exists.
    #[serde(default)]
    pub mp4: Option<PathBuf>,
    /// A conversion cannot survive a quit, so this is not saved.
    #[serde(skip)]
    pub converting: bool,
    #[serde(default)]
    pub convert_error: Option<String>,
    /// Waiting for ffmpeg to be installed.
    #[serde(skip)]
    pub needs_ffmpeg: bool,
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Session cookies of a download that has sat unfinished (paused or failed) this long are dropped from
/// the queue file: they have usually expired anyway, and sending the link from the browser again brings fresh ones.
pub const COOKIE_LIFETIME_SECS: i64 = 7 * 24 * 3600;

/// Drops the cookies of paused or failed items added more than [`COOKIE_LIFETIME_SECS`] before `now`
/// (Unix time). Queued and running items keep theirs.
pub fn expire_cookies(items: &mut [DownloadItem], now: i64) {
    for i in items {
        if i.request.cookies.is_some()
            && i.added < now - COOKIE_LIFETIME_SECS
            && matches!(i.status, Status::Paused | Status::Failed)
        {
            i.request.cookies = None;
        }
    }
}

impl DownloadItem {
    pub fn new(request: JobRequest) -> Self {
        Self {
            id: Uuid::new_v4(),
            request,
            status: Status::Queued,
            destination: None,
            done: 0,
            total: None,
            fraction: None,
            error: None,
            added: chrono::Utc::now().timestamp(),
            speed: 0.0,
            mp4: None,
            converting: false,
            convert_error: None,
            needs_ffmpeg: false,
        }
    }

    pub fn display_name(&self) -> String {
        if let Some(d) = self.destination.as_deref().and_then(Path::file_name) {
            return d.to_string_lossy().into_owned();
        }
        if let Some(f) = self.request.clean_filename() {
            return f;
        }
        self.request
            .valid_url()
            .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).map(str::to_owned))
            .unwrap_or_else(|| self.request.url.clone())
    }

    /// What "Open" and "Show in Folder" act on: the MP4 when there is one.
    pub fn playable(&self) -> Option<&Path> {
        self.mp4.as_deref().or(self.destination.as_deref())
    }

    pub fn can_convert(&self) -> bool {
        self.status == Status::Completed
            && !self.converting
            && self.mp4.is_none()
            && self.destination.as_deref().is_some_and(|d| CONVERTIBLE.contains(&ext_of(d).as_str()))
    }

    pub fn kind(&self) -> Kind {
        let ext = self.playable().map(ext_of).unwrap_or_default();
        if ext.is_empty() {
            return if self.request.is_hls() { Kind::Video } else { Kind::Other };
        }
        if CONVERTIBLE.contains(&ext.as_str()) {
            return Kind::Video;
        }
        match mime_guess::from_ext(&ext).first().map(|m| m.type_().as_str().to_owned()).as_deref() {
            Some("video") => Kind::Video,
            Some("audio") => Kind::Audio,
            _ => Kind::Other,
        }
    }

    pub fn can_pause(&self) -> bool {
        self.status == Status::Downloading
    }

    /// Finished, and not still converting: what "Clear Finished" removes.
    pub fn is_finished(&self) -> bool {
        self.status == Status::Completed && !self.converting
    }

    pub fn eta_seconds(&self) -> Option<f64> {
        let t = self.total?;
        (self.status == Status::Downloading && self.speed > 1.0 && t >= self.done).then(|| (t - self.done) as f64 / self.speed)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub auto_convert: bool,
    pub delete_original: bool,
    pub extension_setup_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { auto_convert: true, delete_original: false, extension_setup_shown: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(path: &str) -> DownloadItem {
        let mut i = DownloadItem::new(JobRequest::new("https://x.example/f"));
        i.status = Status::Completed;
        i.destination = Some(PathBuf::from(path));
        i
    }

    #[test]
    fn only_mpeg_ts_is_convertible() {
        assert!(done("/d/a.ts").can_convert());
        assert!(done("/d/a.M2TS").can_convert());
        assert!(!done("/d/a.zip").can_convert());
        assert!(!done("/d/a.mp4").can_convert());
        let mut converted = done("/d/a.ts");
        converted.mp4 = Some(PathBuf::from("/d/a.mp4"));
        assert!(!converted.can_convert());
    }

    #[test]
    fn kinds() {
        assert_eq!(done("/d/a.ts").kind(), Kind::Video);
        assert_eq!(done("/d/a.mkv").kind(), Kind::Video);
        assert_eq!(done("/d/a.mp3").kind(), Kind::Audio);
        assert_eq!(done("/d/a.flac").kind(), Kind::Audio);
        assert_eq!(done("/d/a.pdf").kind(), Kind::Other);
        assert_eq!(done("/d/README").kind(), Kind::Other);
    }

    #[test]
    fn stale_unfinished_items_lose_their_cookies() {
        let now = 1_800_000_000;
        let item = |status, age_days: i64| {
            let mut i = done("/d/a.mp4");
            i.status = status;
            i.added = now - age_days * 24 * 3600;
            i.request.cookies = Some("s=1".into());
            i
        };
        let mut items = vec![
            item(Status::Paused, 8),
            item(Status::Failed, 8),
            item(Status::Paused, 6),
            item(Status::Queued, 8),
            item(Status::Downloading, 8),
        ];
        expire_cookies(&mut items, now);
        let kept: Vec<bool> = items.iter().map(|i| i.request.cookies.is_some()).collect();
        assert_eq!(kept, [false, false, true, true, true]);
    }

    #[test]
    fn saved_list_round_trips_without_runtime_state() {
        let mut i = done("/d/a.ts");
        i.speed = 5.0;
        i.converting = true;
        let back: DownloadItem = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
        assert_eq!(back.id, i.id);
        assert_eq!(back.status, Status::Completed);
        assert_eq!(back.speed, 0.0);
        assert!(!back.converting);
    }
}
