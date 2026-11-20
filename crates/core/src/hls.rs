//! HLS (`.m3u8`) streams: playlist parsing, and a resumable downloader that joins the segments
//! (and, for streams with a separate audio rendition, merges both tracks into one MP4).

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use futures_util::future::try_join_all;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    DestinationFn, DownloadProgress, Error, ProgressFn, Result,
    downloader::with_retries,
    fmp4, net,
    net::with_suffix,
    paths, probe, remux,
};

/// A segment URL, or a byte range `[start, end)` within it (EXT-X-BYTERANGE: many segments share one file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub url: Url,
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPlaylist {
    pub segments: Vec<Segment>,
    /// EXT-X-MAP (fragmented MP4).
    pub init_segment: Option<Segment>,
    pub encrypted: bool,
    pub is_live: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub bandwidth: u64,
    pub url: Url,
    /// GROUP-ID of the EXT-X-MEDIA audio renditions this variant plays with.
    pub audio_group: Option<String>,
}

/// An EXT-X-MEDIA:TYPE=AUDIO entry. No URL means the audio is muxed into the variant itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRendition {
    pub group_id: String,
    pub url: Option<Url>,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Master { variants: Vec<Variant>, audio: Vec<AudioRendition> },
    Media(MediaPlaylist),
}

/// Reads `NAME=value` or `NAME="value"` out of an HLS tag line. The name must start an attribute
/// (after `:` or `,`), not be the tail of another one (BANDWIDTH inside AVERAGE-BANDWIDTH).
pub fn attribute(name: &str, line: &str) -> Option<String> {
    let key = format!("{name}=");
    let mut from = 0;
    while let Some(i) = line[from..].find(&key).map(|i| i + from) {
        let starts_attribute = i > 0 && matches!(line.as_bytes()[i - 1], b':' | b',');
        if starts_attribute {
            let rest = &line[i + key.len()..];
            return Some(match rest.strip_prefix('"') {
                Some(body) => body.split('"').next()?.to_owned(),
                None => rest.split(',').next().unwrap_or("").to_owned(),
            });
        }
        from = i + key.len();
    }
    None
}

/// `n[@o]` of EXT-X-BYTERANGE.
fn byte_range(spec: &str) -> Option<(u64, Option<u64>)> {
    let mut parts = spec.trim().splitn(2, '@');
    let n: u64 = parts.next()?.parse().ok().filter(|&n| n > 0)?;
    Some((n, parts.next().and_then(|o| o.parse().ok())))
}

pub fn parse(text: &str, base: &Url) -> Parsed {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let resolve = |s: &str| base.join(s).ok();

    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let mut variants = Vec::new();
        let mut audio = Vec::new();
        let mut pending: Option<(u64, Option<String>)> = None;
        for l in &lines {
            if l.starts_with("#EXT-X-STREAM-INF") {
                let bw = attribute("BANDWIDTH", l).and_then(|b| b.parse().ok()).unwrap_or(0);
                pending = Some((bw, attribute("AUDIO", l)));
            } else if l.starts_with("#EXT-X-MEDIA:") && attribute("TYPE", l).as_deref() == Some("AUDIO") {
                if let Some(group_id) = attribute("GROUP-ID", l) {
                    audio.push(AudioRendition {
                        group_id,
                        url: attribute("URI", l).and_then(|u| resolve(&u)),
                        is_default: attribute("DEFAULT", l).as_deref() == Some("YES"),
                    });
                }
            } else if !l.is_empty()
                && !l.starts_with('#')
                && let (Some((bandwidth, audio_group)), Some(url)) = (pending.take(), resolve(l))
            {
                variants.push(Variant { bandwidth, url, audio_group });
            }
        }
        return Parsed::Master { variants, audio };
    }

    let mut segments = Vec::new();
    let mut init_segment = None;
    let mut encrypted = false;
    let mut ended = false;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    // A range without "@offset" continues where the previous one in the same file ended.
    let mut range_end: HashMap<Url, u64> = HashMap::new();
    let mut segment = |u: Url, br: Option<(u64, Option<u64>)>| -> Segment {
        let Some((len, offset)) = br else { return Segment { url: u, range: None } };
        let start = offset.or_else(|| range_end.get(&u).copied()).unwrap_or(0);
        range_end.insert(u.clone(), start + len);
        Segment { url: u, range: Some((start, start + len)) }
    };
    for l in &lines {
        if l.starts_with("#EXT-X-KEY") || l.starts_with("#EXT-X-SESSION-KEY") {
            if attribute("METHOD", l).is_some_and(|m| m != "NONE") {
                encrypted = true;
            }
        } else if l.starts_with("#EXT-X-MAP") {
            if let Some(u) = attribute("URI", l).and_then(|u| resolve(&u)) {
                let br = attribute("BYTERANGE", l).and_then(|b| byte_range(&b)).map(|(n, o)| (n, Some(o.unwrap_or(0))));
                init_segment = Some(segment(u, br));
            }
        } else if let Some(spec) = l.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = byte_range(spec);
        } else if l.starts_with("#EXT-X-ENDLIST") {
            ended = true;
        } else if !l.is_empty()
            && !l.starts_with('#')
            && let Some(u) = resolve(l)
        {
            segments.push(segment(u, pending_range.take()));
        }
    }
    Parsed::Media(MediaPlaylist { segments, init_segment, encrypted, is_live: !ended })
}

/// The requested variant if the master lists it, else the highest-bandwidth one.
pub fn pick<'a>(variants: &'a [Variant], preferred: Option<&Url>) -> Option<&'a Variant> {
    preferred
        .and_then(|p| variants.iter().find(|v| &v.url == p))
        .or_else(|| variants.iter().rev().max_by_key(|v| v.bandwidth))
}

/// The separate audio playlist a variant needs, or `None` when its audio is muxed in (or it has none).
pub fn audio_url(variant: &Variant, renditions: &[AudioRendition]) -> Option<Url> {
    let group = variant.audio_group.as_deref()?;
    let members: Vec<_> = renditions.iter().filter(|r| r.group_id == group).collect();
    members.iter().find(|r| r.is_default).or(members.first()).and_then(|r| r.url.clone())
}

/// How far one track (video, audio, or the only one) of an HLS download got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackState {
    /// Playlist length, to tell whether the stream changed.
    pub segments: usize,
    /// Segments fully written, in playlist order.
    pub done: usize,
    /// Track file length after those segments (and the init segment).
    pub bytes: u64,
    /// Next fMP4 fragment number.
    pub sequence: u32,
}

impl TrackState {
    fn new(segments: usize) -> Self {
        Self { segments, done: 0, bytes: 0, sequence: 1 }
    }
}

/// Persisted as `<final name>.hoardini` so a paused or interrupted HLS download can continue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub tracks: Vec<TrackState>,
}

impl State {
    /// Saved progress for this exact stream, or `None` when there is none or the playlists changed.
    fn load(path: &Path, source: &Url, variant: Option<&Url>, counts: &[usize]) -> Option<Self> {
        if fs::metadata(path).ok()?.len() > 1 << 20 {
            return None;
        }
        let s: State = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let ok = s.source == source.as_str()
            && s.variant.as_deref() == variant.map(Url::as_str)
            && s.tracks.iter().map(|t| t.segments).eq(counts.iter().copied())
            && s.tracks.iter().all(|t| t.done <= t.segments && t.sequence > 0);
        ok.then_some(s)
    }

    fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_vec(self) {
            let _ = paths::write_atomic(path, &data, 0o600);
        }
    }
}

/// Progress across every track of an HLS download, counted in segments.
struct SegmentCounter {
    total: usize,
    progress: ProgressFn,
    inner: Mutex<(i64, i64)>,
}

impl SegmentCounter {
    fn add(&self, bytes: i64, segments: i64) {
        let p = {
            let mut g = self.inner.lock().unwrap();
            g.0 += bytes;
            g.1 += segments;
            DownloadProgress {
                done: g.0.max(0) as u64,
                total: None,
                fraction_override: Some(g.1.max(0) as f64 / self.total as f64),
            }
        };
        (self.progress)(p);
    }
}

const PARALLEL: usize = 6;
const MAX_PLAYLIST: usize = 16 << 20;

pub struct HlsDownloader {
    pub url: Url,
    pub directory: PathBuf,
    pub headers: Vec<(String, String)>,
    pub filename: Option<String>,
    /// A variant playlist chosen in the browser; `url` is then its master, which knows the audio track.
    pub variant: Option<Url>,
    pub on_destination: Option<DestinationFn>,
    client: Client,
}

impl HlsDownloader {
    pub fn new(url: Url, directory: impl Into<PathBuf>, headers: Vec<(String, String)>, filename: Option<String>, variant: Option<Url>) -> Self {
        Self { url, directory: directory.into(), headers, filename, variant, on_destination: None, client: net::client() }
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    pub async fn run(&self, progress: ProgressFn) -> Result<PathBuf> {
        let mut playlist_url = self.url.clone();
        let mut media = None;
        let mut audio_url: Option<Url> = None;
        for _ in 0..3 {
            // master -> variant -> (rarely) nested
            match parse(&self.fetch_text(&playlist_url).await?, &playlist_url) {
                Parsed::Master { variants, audio } => {
                    let chosen = pick(&variants, self.variant.as_ref()).ok_or(Error::BadPlaylist)?;
                    playlist_url = chosen.url.clone();
                    audio_url = self::audio_url(chosen, &audio).or(audio_url);
                }
                Parsed::Media(m) => {
                    media = Some(m);
                    break;
                }
            }
        }
        // Sites like Vimeo serve video-only variants with the audio as a separate rendition.
        let mut audio = match &audio_url {
            Some(u) => match parse(&self.fetch_text(u).await?, u) {
                Parsed::Media(a) => Some(a),
                Parsed::Master { .. } => return Err(Error::BadPlaylist),
            },
            None => None,
        };
        let media = media.filter(|m| !m.segments.is_empty()).ok_or(Error::BadPlaylist)?;
        for m in std::iter::once(&media).chain(audio.as_ref()) {
            if m.encrypted {
                return Err(Error::EncryptedStream);
            }
            if m.is_live {
                return Err(Error::LiveStream);
            }
        }
        if audio.as_ref().is_some_and(|a| a.segments.is_empty()) {
            audio = None;
        }
        if audio.is_some() && remux::ffmpeg_path().is_none() {
            return Err(Error::FfmpegMissing);
        }

        let video_ext = if media.init_segment.is_some() { "mp4" } else { "ts" };
        let ext = if audio.is_some() { "mp4" } else { video_ext };
        let stem = strip_known_ext(self.filename.clone().unwrap_or_else(|| self.default_name()), &[".ts", ".mp4", ".m4v"]);
        let name = format!("{}.{ext}", probe::sanitize(&stem));
        let (final_path, _lock) = net::claim(&self.directory, &name)?;
        if let Some(d) = &self.on_destination {
            d(&final_path);
        }

        let part = with_suffix(&final_path, ".part");
        // Each track of a split stream goes to its own hidden temp file, then both are muxed into the .part file.
        let tracks: Vec<(&MediaPlaylist, PathBuf)> = match &audio {
            Some(a) => vec![
                (&media, net::track_file(&final_path, "video", video_ext)),
                (a, net::track_file(&final_path, "audio", audio_extension(a))),
            ],
            None => vec![(&media, part.clone())],
        };

        // Pick up where a paused or interrupted download stopped, if it was the same stream.
        let state_path = with_suffix(&final_path, ".hoardini");
        let counts: Vec<usize> = tracks.iter().map(|t| t.0.segments.len()).collect();
        let mut state = State::load(&state_path, &self.url, self.variant.as_ref(), &counts).unwrap_or_else(|| State {
            source: self.url.to_string(),
            variant: self.variant.as_ref().map(Url::to_string),
            tracks: counts.iter().map(|&n| TrackState::new(n)).collect(),
        });
        state.save(&state_path);
        let counter = SegmentCounter {
            total: counts.iter().sum::<usize>().max(1),
            progress,
            inner: Mutex::new((
                state.tracks.iter().map(|t| t.bytes as i64).sum(),
                state.tracks.iter().map(|t| t.done as i64).sum(),
            )),
        };
        counter.add(0, 0);
        for (k, (m, file)) in tracks.iter().enumerate() {
            self.write(m, file, k, &mut state, &state_path, &counter).await?;
        }
        if audio.is_some() {
            let _ = fs::remove_file(&part);
            remux::merge(&tracks[0].1, &tracks[1].1, &part).await?;
            for (_, f) in &tracks {
                let _ = fs::remove_file(f);
            }
        }
        net::mark_origin(&part, &self.url, net::header_value(&self.headers, "Referer"));
        fs::rename(&part, &final_path)?;
        let _ = fs::remove_file(&state_path);
        Ok(final_path)
    }

    /// Appends the playlist's segments in order to `file`, continuing after the ones `state` says are done.
    /// The file is kept on failure or pause so the download can resume.
    async fn write(&self, media: &MediaPlaylist, file: &Path, k: usize, state: &mut State, state_path: &Path, counter: &SegmentCounter) -> Result<()> {
        let mut t = state.tracks[k];
        let on_disk = fs::metadata(file).map(|m| m.len()).ok();
        if t.done > 0 && on_disk.is_none_or(|n| n < t.bytes) {
            // The track file is gone or short: this track starts over.
            counter.add(-(t.bytes as i64), -(t.done as i64));
            t = TrackState::new(t.segments);
        }
        let mut out = OpenOptions::new().write(true).create(true).truncate(t.done == 0).open(file)?;
        // Bytes past the last saved segment are from a batch that was cut off; drop them.
        out.set_len(t.bytes)?;
        out.seek(SeekFrom::End(0))?;

        if t.done == 0
            && let Some(init) = &media.init_segment
        {
            let d = self.fetch(init).await?;
            out.write_all(&d)?;
            t.bytes = d.len() as u64;
            counter.add(d.len() as i64, 0);
        }
        // Download in batches of PARALLEL segments, writing each batch in playlist order.
        let mut i = t.done;
        while i < media.segments.len() {
            let batch = &media.segments[i..(i + PARALLEL).min(media.segments.len())];
            let datas = try_join_all(batch.iter().map(|s| self.fetch(s))).await?;
            let mut added = 0;
            for mut d in datas {
                // fMP4 segments get fresh fragment numbers so players read past the first one.
                if media.init_segment.is_some() {
                    fmp4::renumber_fragments(&mut d, &mut t.sequence);
                }
                out.write_all(&d)?;
                t.bytes += d.len() as u64;
                added += d.len();
            }
            i += batch.len();
            t.done = i;
            state.tracks[k] = t;
            state.save(state_path);
            counter.add(added as i64, batch.len() as i64);
        }
        out.sync_data()?;
        Ok(())
    }

    fn default_name(&self) -> String {
        let last = self.url.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
        let stem = probe::split_ext(last).0.to_lowercase();
        if !["", "index", "playlist", "master", "main", "manifest", "stream"].contains(&stem.as_str()) {
            return stem;
        }
        format!("video-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"))
    }

    async fn fetch_text(&self, u: &Url) -> Result<String> {
        let data = self.fetch_bytes(u, None).await?;
        if data.len() > MAX_PLAYLIST {
            return Err(Error::BadPlaylist);
        }
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    async fn fetch(&self, s: &Segment) -> Result<Vec<u8>> {
        self.fetch_bytes(&s.url, s.range).await
    }

    async fn fetch_bytes(&self, u: &Url, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        with_retries(|| async {
            let header = range.map(|(a, b)| format!("bytes={a}-{}", b - 1));
            let resp = net::get(&self.client, u, &self.headers, &self.url, header).await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(Error::BadResponse(status.as_u16()));
            }
            let data = resp.bytes().await?.to_vec();
            let Some((a, b)) = range else { return Ok(data) };
            let want = (b - a) as usize;
            if status == StatusCode::PARTIAL_CONTENT {
                if data.len() != want {
                    return Err(Error::SizeMismatch { expected: want as u64, got: data.len() as u64 });
                }
                return Ok(data);
            }
            // Server ignored Range and sent the whole file: cut the segment out of it.
            if (data.len() as u64) < b {
                return Err(Error::RangeIgnored);
            }
            Ok(data[a as usize..b as usize].to_vec())
        })
        .await
    }
}

/// Only strip an extension we would re-add; "Lesson 1.5 intro" must stay intact.
pub(crate) fn strip_known_ext(mut stem: String, known: &[&str]) -> String {
    for k in known {
        if stem.to_lowercase().ends_with(k) {
            stem.truncate(stem.len() - k.len());
        }
    }
    stem
}

/// Packed audio keeps its own extension (.aac, .mp3) so ffmpeg picks the right reader.
pub fn audio_extension(m: &MediaPlaylist) -> &'static str {
    if m.init_segment.is_some() {
        return "m4a";
    }
    let ext = m.segments.first().map(|s| probe::path_extension(s.url.path()).to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "aac" => "aac",
        "mp3" => "mp3",
        "ac3" => "ac3",
        "ec3" => "ec3",
        _ => "ts",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/v/master.m3u8").unwrap()
    }

    #[test]
    fn parses_master_with_audio_groups() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=NO,URI=\"audio/alt.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio/main.m3u8\"\n\
            #EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=999999,BANDWIDTH=800000,CODECS=\"avc1,mp4a\",AUDIO=\"aud\"\n\
            low/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,AUDIO=\"aud\"\n\
            https://other.example/high.m3u8\n";
        let Parsed::Master { variants, audio } = parse(text, &base()) else { panic!("master") };
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].bandwidth, 800000, "AVERAGE-BANDWIDTH is not BANDWIDTH");
        assert_eq!(variants[0].url.as_str(), "https://cdn.example/v/low/index.m3u8");
        let best = pick(&variants, None).unwrap();
        assert_eq!(best.url.as_str(), "https://other.example/high.m3u8");
        let preferred = Url::parse("https://cdn.example/v/low/index.m3u8").unwrap();
        assert_eq!(pick(&variants, Some(&preferred)).unwrap().bandwidth, 800000);
        assert_eq!(audio_url(best, &audio).unwrap().as_str(), "https://cdn.example/v/audio/main.m3u8");
    }

    #[test]
    fn parses_media_with_map_and_byteranges() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"700@0\"\n\
            #EXTINF:6,\n#EXT-X-BYTERANGE:1000@700\nall.mp4\n\
            #EXTINF:6,\n#EXT-X-BYTERANGE:500\nall.mp4\n\
            #EXTINF:6,\nseg3.cmfv\n#EXT-X-ENDLIST\n";
        let Parsed::Media(m) = parse(text, &base()) else { panic!("media") };
        assert_eq!(m.init_segment.as_ref().unwrap().range, Some((0, 700)));
        assert_eq!(m.segments[0].range, Some((700, 1700)));
        assert_eq!(m.segments[1].range, Some((1700, 2200)), "continues after the previous range");
        assert_eq!(m.segments[2].range, None);
        assert!(!m.is_live && !m.encrypted);
        assert_eq!(audio_extension(&m), "m4a");
    }

    #[test]
    fn detects_encryption_and_live() {
        let Parsed::Media(m) = parse("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\na.ts\n", &base()) else { panic!() };
        assert!(m.encrypted && m.is_live);
        let Parsed::Media(m) = parse("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\na.aac\n#EXT-X-ENDLIST\n", &base()) else { panic!() };
        assert!(!m.encrypted && !m.is_live);
        assert_eq!(audio_extension(&m), "aac");
    }

    #[test]
    fn attribute_values() {
        let l = "#EXT-X-STREAM-INF:BANDWIDTH=5,CODECS=\"a,b\",RESOLUTION=1x2";
        assert_eq!(attribute("CODECS", l).as_deref(), Some("a,b"));
        assert_eq!(attribute("RESOLUTION", l).as_deref(), Some("1x2"));
        assert_eq!(attribute("AUDIO", l), None);
    }

    #[test]
    fn strips_only_known_extensions() {
        assert_eq!(strip_known_ext("Lesson 1.5 intro".into(), &[".ts", ".mp4"]), "Lesson 1.5 intro");
        assert_eq!(strip_known_ext("clip.MP4".into(), &[".ts", ".mp4"]), "clip");
    }

    #[test]
    fn resume_state_must_match_stream() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.hoardini");
        let src = base();
        let s = State { source: src.to_string(), variant: None, tracks: vec![TrackState { segments: 10, done: 4, bytes: 400, sequence: 5 }] };
        s.save(&p);
        assert_eq!(State::load(&p, &src, None, &[10]), Some(s.clone()));
        assert_eq!(State::load(&p, &src, None, &[11]), None, "segment count changed");
        assert_eq!(State::load(&p, &src, Some(&src), &[10]), None, "variant changed");
        let bad = State { tracks: vec![TrackState { segments: 10, done: 11, bytes: 0, sequence: 1 }], ..s };
        bad.save(&p);
        assert_eq!(State::load(&p, &src, None, &[10]), None, "done past the end");
    }
}
