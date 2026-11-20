use serde::{Deserialize, Serialize};
use url::Url;

use crate::probe;

/// A download request as sent by the browser extension (via the native host) or typed into the app.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// "file" | "hls"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// HLS only: the variant playlist picked in the popup, with `url` being its master.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// File downloads only: a separate audio-only file to merge with the video-only `url` (DASH, e.g. Facebook).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
}

/// Only plain http(s) URLs with a host are accepted.
pub fn http_url(s: &str) -> Option<Url> {
    let u = Url::parse(s.trim()).ok()?;
    (matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())).then_some(u)
}

impl JobRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Default::default() }
    }

    pub fn valid_url(&self) -> Option<Url> {
        http_url(&self.url)
    }

    pub fn valid_variant(&self) -> Option<Url> {
        self.variant.as_deref().and_then(http_url)
    }

    pub fn valid_audio(&self) -> Option<Url> {
        if self.is_hls() { None } else { self.audio.as_deref().and_then(http_url) }
    }

    pub fn is_hls(&self) -> bool {
        self.kind.as_deref() == Some("hls")
            || self.valid_url().is_some_and(|u| probe::path_extension(u.path()).eq_ignore_ascii_case("m3u8"))
    }

    /// Header values containing line breaks (or other control characters) are dropped: header injection.
    pub fn headers(&self) -> Vec<(String, String)> {
        [("Referer", &self.referrer), ("Cookie", &self.cookies), ("User-Agent", &self.user_agent)]
            .into_iter()
            .filter_map(|(name, value)| {
                let v = value.as_deref()?;
                (!v.is_empty() && !v.chars().any(|c| c.is_control() && c != '\t')).then(|| (name.to_owned(), v.to_owned()))
            })
            .collect()
    }

    pub fn clean_filename(&self) -> Option<String> {
        let f = self.filename.as_deref()?;
        let s: String = probe::strip_invisible(f).chars().take(150).collect();
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_http_urls() {
        assert!(JobRequest::new("https://example.com/a.zip").valid_url().is_some());
        assert!(JobRequest::new("ftp://example.com/a.zip").valid_url().is_none());
        assert!(JobRequest::new("file:///etc/passwd").valid_url().is_none());
        assert!(JobRequest::new("javascript:alert(1)").valid_url().is_none());
        assert!(JobRequest::new("").valid_url().is_none());
    }

    #[test]
    fn hls_by_kind_or_extension() {
        assert!(JobRequest::new("https://x.example/v/master.m3u8?sig=1").is_hls());
        assert!(JobRequest { kind: Some("hls".into()), ..JobRequest::new("https://x.example/play") }.is_hls());
        assert!(!JobRequest::new("https://x.example/a.mp4").is_hls());
    }

    #[test]
    fn audio_ignored_for_hls() {
        let j = JobRequest { audio: Some("https://x.example/a.mp4".into()), ..JobRequest::new("https://x.example/m.m3u8") };
        assert!(j.valid_audio().is_none());
        let j = JobRequest { audio: Some("https://x.example/a.mp4".into()), ..JobRequest::new("https://x.example/v.mp4") };
        assert!(j.valid_audio().is_some());
        let j = JobRequest { audio: Some(String::new()), ..JobRequest::new("https://x.example/v.mp4") };
        assert!(j.valid_audio().is_none());
    }

    #[test]
    fn drops_header_injection() {
        let j = JobRequest {
            referrer: Some("https://a.example/\r\nX-Evil: 1".into()),
            cookies: Some("a=b".into()),
            user_agent: Some(String::new()),
            ..JobRequest::new("https://a.example/f")
        };
        assert_eq!(j.headers(), vec![("Cookie".to_owned(), "a=b".to_owned())]);
    }

    #[test]
    fn decodes_extension_message() {
        let j: JobRequest = serde_json::from_str(
            r#"{"url":"https://a.example/v.mp4","referrer":"","kind":"file","filename":" Title\u202e ","variant":"","audio":"","cookies":"","userAgent":"UA"}"#,
        )
        .unwrap();
        assert_eq!(j.user_agent.as_deref(), Some("UA"));
        assert_eq!(j.clean_filename().as_deref(), Some("Title"));
        assert_eq!(j.headers(), vec![("User-Agent".to_owned(), "UA".to_owned())]);
    }
}
