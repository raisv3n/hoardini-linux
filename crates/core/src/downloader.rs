use std::{
    fs::{self, OpenOptions},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::future::try_join_all;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    DestinationFn, Error, Result, net,
    net::with_suffix,
    paths,
    probe::{self, ProbeResult},
    segments::{self, Segment},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct DownloadProgress {
    pub done: u64,
    pub total: Option<u64>,
    /// Set when progress is not byte-based (HLS counts segments).
    pub fraction_override: Option<f64>,
}

impl DownloadProgress {
    pub fn fraction(&self) -> Option<f64> {
        self.fraction_override
            .or_else(|| self.total.filter(|&t| t > 0).map(|t| (self.done as f64 / t as f64).min(1.0)))
    }
}

pub type ProgressFn = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

const MAX_RETRIES: u32 = 3;
const BUFFER: usize = 128 * 1024;

/// Retries `f` up to three times with a growing pause (1 s, 2 s, 3 s).
pub(crate) async fn with_retries<T, F, Fut>(mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                attempt += 1;
                // Asking again gets the same oversized answer.
                if attempt > MAX_RETRIES || matches!(e, Error::TooLarge) {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_secs(attempt.into())).await;
            }
        }
    }
}

/// Persisted next to the partial file (`<name>.hoardini`) so an interrupted download can continue.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DownloadState {
    url: String,
    length: u64,
    #[serde(default)]
    validator: Option<String>,
    segments: Vec<Segment>,
}

impl DownloadState {
    /// The file is fully covered by contiguous segments and no segment claims more than its size.
    fn is_consistent(&self) -> bool {
        let mut next = 0;
        for s in &self.segments {
            if s.start != next || s.end < s.start || s.written > s.end - s.start + 1 {
                return false;
            }
            next = s.end + 1;
        }
        next == self.length && !self.segments.is_empty()
    }
}

struct StoreInner {
    state: DownloadState,
    last_save: Instant,
    finished: bool,
}

/// Resume state shared by the segment downloads. Saved at most once a second while bytes arrive,
/// at the end of each segment, and when the download stops (dropped on pause or failure).
struct SegmentStore {
    inner: Mutex<StoreInner>,
    path: PathBuf,
}

impl SegmentStore {
    fn new(state: DownloadState, path: PathBuf) -> Self {
        Self { inner: Mutex::new(StoreInner { state, last_save: Instant::now(), finished: false }), path }
    }

    fn segment(&self, i: usize) -> Segment {
        self.inner.lock().unwrap().state.segments[i]
    }

    fn done(&self) -> u64 {
        self.inner.lock().unwrap().state.segments.iter().map(|s| s.written).sum()
    }

    fn add(&self, n: u64, i: usize) {
        let mut g = self.inner.lock().unwrap();
        g.state.segments[i].written += n;
        if g.last_save.elapsed() > Duration::from_secs(1) {
            Self::write(&self.path, &mut g);
        }
    }

    fn save(&self) {
        Self::write(&self.path, &mut self.inner.lock().unwrap());
    }

    fn write(path: &Path, g: &mut StoreInner) {
        g.last_save = Instant::now();
        if g.finished {
            return;
        }
        if let Ok(data) = serde_json::to_vec(&g.state) {
            let _ = paths::write_atomic(path, &data, 0o600);
        }
    }

    /// The download is complete: no more saves (the state file is about to be deleted).
    fn finish(&self) {
        self.inner.lock().unwrap().finished = true;
    }
}

impl Drop for SegmentStore {
    fn drop(&mut self) {
        // Paused (task aborted) or failed: keep the resume state exact.
        if let Ok(mut g) = self.inner.lock() {
            Self::write(&self.path, &mut g);
        }
    }
}

/// A segmented, resumable download of one file (SPEC.md).
pub struct Downloader {
    pub url: Url,
    pub directory: PathBuf,
    pub connections: usize,
    /// Extra request headers (e.g. Cookie, Referer, User-Agent) sent on every request.
    pub headers: Vec<(String, String)>,
    pub filename: Option<String>,
    /// Used as-is instead of a sanitized name: hidden track files of a paired download start with ".".
    pub exact_name: Option<String>,
    pub on_destination: Option<DestinationFn>,
    client: Client,
}

impl Downloader {
    pub fn new(url: Url, directory: impl Into<PathBuf>, headers: Vec<(String, String)>, filename: Option<String>) -> Self {
        Self {
            url,
            directory: directory.into(),
            connections: 8,
            headers,
            filename,
            exact_name: None,
            on_destination: None,
            client: net::client(),
        }
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Downloads to `directory`, returning the final file path.
    pub async fn run(&self, progress: ProgressFn) -> Result<PathBuf> {
        let probe = probe::run(&self.client, &self.url, &self.headers, &self.url).await?;
        let name = self.exact_name.clone().unwrap_or_else(|| probe::final_name(self.filename.as_deref(), &probe.filename));
        let (final_path, _lock) = net::claim(&self.directory, &name)?;
        if let Some(d) = &self.on_destination {
            d(&final_path);
        }
        let part = with_suffix(&final_path, ".part");
        let state_path = with_suffix(&final_path, ".hoardini");

        let (true, Some(length)) = (probe.accepts_ranges, probe.length) else {
            self.single_stream(&probe, &part, &final_path, &progress).await?;
            return Ok(final_path);
        };

        let mut state = load_state(&state_path, &probe, length, part.is_file()).unwrap_or_else(|| DownloadState {
            url: probe.final_url.to_string(),
            length,
            validator: probe.validator.clone(),
            segments: segments::plan(length, self.connections.max(1), segments::MIN_SEGMENT),
        });
        if state.segments.iter().all(|s| s.written == 0) {
            let f = OpenOptions::new().write(true).create(true).truncate(true).open(&part)?;
            f.set_len(length)?;
        }
        state.url = probe.final_url.to_string();

        let pending: Vec<usize> = (0..state.segments.len()).filter(|&i| !state.segments[i].is_done()).collect();
        let store = Arc::new(SegmentStore::new(state, state_path.clone()));
        store.save();
        progress(DownloadProgress { done: store.done(), total: Some(length), ..Default::default() });

        try_join_all(pending.into_iter().map(|i| {
            let store = store.clone();
            let progress = progress.clone();
            let (part, url) = (&part, &probe.final_url);
            async move {
                let on_bytes = |done| progress(DownloadProgress { done, total: Some(length), ..Default::default() });
                with_retries(|| self.stream_segment(i, &store, part, url, &on_bytes)).await
            }
        }))
        .await?;

        let got = store.done();
        if got != length {
            return Err(Error::SizeMismatch { expected: length, got });
        }
        store.finish();
        net::mark_origin(&part, &probe.final_url, net::header_value(&self.headers, "Referer"));
        fs::rename(&part, &final_path)?;
        let _ = fs::remove_file(&state_path);
        Ok(final_path)
    }

    async fn stream_segment(&self, i: usize, store: &SegmentStore, part: &Path, url: &Url, on_bytes: &(dyn Fn(u64) + Sync)) -> Result<()> {
        let seg = store.segment(i);
        if seg.is_done() {
            return Ok(());
        }
        let range = format!("bytes={}-{}", seg.next_offset(), seg.end);
        let mut resp = net::get(&self.client, url, &self.headers, &self.url, Some(range)).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => return Err(Error::RangeIgnored),
            s => return Err(Error::BadResponse(s.as_u16())),
        }
        // Bytes from a different range than requested would land at the wrong offset.
        let start = resp
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().strip_prefix("bytes ")?.split('-').next()?.trim().parse::<u64>().ok());
        if start.is_some_and(|s| s != seg.next_offset()) {
            return Err(Error::RangeIgnored);
        }

        let file = OpenOptions::new().write(true).open(part)?;
        let mut offset = seg.next_offset();
        let mut buffer: Vec<u8> = Vec::with_capacity(BUFFER);
        let flush = |buffer: &mut Vec<u8>, offset: &mut u64| -> Result<()> {
            if buffer.is_empty() {
                return Ok(());
            }
            file.write_all_at(buffer, *offset)?;
            *offset += buffer.len() as u64;
            store.add(buffer.len() as u64, i);
            on_bytes(store.done());
            buffer.clear();
            Ok(())
        };
        while let Some(chunk) = resp.chunk().await? {
            // Never write past the segment, whatever the server sends.
            let room = (seg.end + 1 - offset) as usize - buffer.len();
            buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if buffer.len() >= BUFFER {
                flush(&mut buffer, &mut offset)?;
            }
            if offset + buffer.len() as u64 > seg.end {
                break;
            }
        }
        flush(&mut buffer, &mut offset)?;
        store.save();
        let now = store.segment(i);
        if !now.is_done() {
            // The server closed the connection early; the retry continues from here.
            return Err(Error::SizeMismatch { expected: now.end - now.start + 1, got: now.written });
        }
        Ok(())
    }

    /// No range support: one plain stream, no resume.
    async fn single_stream(&self, probe: &ProbeResult, part: &Path, final_path: &Path, progress: &ProgressFn) -> Result<()> {
        let url = &probe.final_url;
        let mut resp = net::get(&self.client, url, &self.headers, &self.url, None).await?;
        if !resp.status().is_success() {
            return Err(Error::BadResponse(resp.status().as_u16()));
        }
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(part)?;
        let _ = fs::remove_file(with_suffix(final_path, ".hoardini"));
        let mut done: u64 = 0;
        let mut buffer: Vec<u8> = Vec::with_capacity(BUFFER);
        while let Some(chunk) = resp.chunk().await? {
            buffer.extend_from_slice(&chunk);
            if buffer.len() >= BUFFER {
                file.write_all_at(&buffer, done)?;
                done += buffer.len() as u64;
                buffer.clear();
                progress(DownloadProgress { done, total: probe.length, ..Default::default() });
            }
        }
        file.write_all_at(&buffer, done)?;
        done += buffer.len() as u64;
        drop(file);
        progress(DownloadProgress { done, total: probe.length, ..Default::default() });
        if let Some(expected) = probe.length.filter(|&l| l != done) {
            return Err(Error::SizeMismatch { expected, got: done });
        }
        net::mark_origin(part, url, net::header_value(&self.headers, "Referer"));
        fs::rename(part, final_path)?;
        Ok(())
    }
}

fn load_state(path: &Path, probe: &ProbeResult, length: u64, part_exists: bool) -> Option<DownloadState> {
    if !part_exists || fs::metadata(path).ok()?.len() > 1 << 20 {
        return None;
    }
    let s: DownloadState = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (s.length == length && s.validator == probe.validator && s.is_consistent()).then_some(s)
}
