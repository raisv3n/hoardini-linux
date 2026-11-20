//! A video-only file and an audio-only file (DASH representations, e.g. Facebook's 720p) downloaded
//! side by side and merged into one MP4. Each half is an ordinary segmented, resumable file download.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use reqwest::Client;
use url::Url;

use crate::{
    DestinationFn, DownloadProgress, Downloader, Error, ProgressFn, Result, hls::strip_known_ext, net,
    net::with_suffix, probe, remux,
};

pub struct PairDownloader {
    pub video: Url,
    pub audio: Url,
    pub directory: PathBuf,
    pub headers: Vec<(String, String)>,
    pub filename: Option<String>,
    pub on_destination: Option<DestinationFn>,
    client: Client,
}

impl PairDownloader {
    pub fn new(video: Url, audio: Url, directory: impl Into<PathBuf>, headers: Vec<(String, String)>, filename: Option<String>) -> Self {
        Self { video, audio, directory: directory.into(), headers, filename, on_destination: None, client: net::client() }
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// The browser attached cookies for the video's host; they must not travel to a different audio host.
    fn audio_headers(&self) -> Vec<(String, String)> {
        net::headers_for(&self.headers, &self.audio, &self.video)
    }

    pub async fn run(&self, progress: ProgressFn) -> Result<PathBuf> {
        if remux::ffmpeg_path().is_none() {
            return Err(Error::FfmpegMissing);
        }
        let stem = self.filename.clone().unwrap_or_else(|| {
            let last = self.video.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
            probe::split_ext(last).0.to_owned()
        });
        let stem = strip_known_ext(stem, &[".mp4", ".m4v", ".mov"]);
        let (final_path, _lock) = net::claim(&self.directory, &format!("{}.mp4", probe::sanitize(&stem)))?;
        if let Some(d) = &self.on_destination {
            d(&final_path);
        }

        // Both sizes up front, so progress runs over the two files from the start.
        let audio_headers = self.audio_headers();
        let (pv, pa) = tokio::try_join!(
            probe::run(&self.client, &self.video, &self.headers, &self.video),
            probe::run(&self.client, &self.audio, &audio_headers, &self.video),
        )?;
        let total = pv.length.zip(pa.length).map(|(v, a)| v + a);
        let done = Arc::new(Mutex::new([0u64; 2]));
        let set = move |k: usize, n: u64| {
            let p = {
                let mut d = done.lock().unwrap();
                d[k] = n;
                DownloadProgress { done: d[0] + d[1], total, ..Default::default() }
            };
            progress(p);
        };
        let set = Arc::new(set);

        let (video_file, audio_file) = tokio::try_join!(
            self.track(&self.video, &self.headers, net::track_file(&final_path, "video", "mp4"), 0, set.clone()),
            self.track(&self.audio, &audio_headers, net::track_file(&final_path, "audio", "mp4"), 1, set.clone()),
        )?;

        let part = with_suffix(&final_path, ".part");
        let _ = fs::remove_file(&part);
        remux::merge(&video_file, &audio_file, &part).await?;
        net::mark_origin(&part, &self.video, net::header_value(&self.headers, "Referer"));
        fs::rename(&part, &final_path)?;
        for f in [video_file, audio_file] {
            let _ = fs::remove_file(f);
        }
        Ok(final_path)
    }

    /// Downloads one half to its hidden track file, or reuses it when an earlier run finished it
    /// (a finished track has no .hoardini resume state next to it).
    async fn track(
        &self,
        url: &Url,
        headers: &[(String, String)],
        file: PathBuf,
        k: usize,
        set: Arc<dyn Fn(usize, u64) + Send + Sync>,
    ) -> Result<PathBuf> {
        if file.is_file() && !with_suffix(&file, ".hoardini").exists() {
            set(k, fs::metadata(&file).map(|m| m.len()).unwrap_or(0));
            return Ok(file);
        }
        let dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut d = Downloader::new(url.clone(), dir, headers.to_vec(), None).with_client(self.client.clone());
        d.exact_name = file.file_name().map(|n| n.to_string_lossy().into_owned());
        d.run(Arc::new(move |p| set(k, p.done))).await
    }
}
