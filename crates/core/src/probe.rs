use percent_encoding::percent_decode_str;
use reqwest::{Client, StatusCode, header};
use url::Url;

use crate::{Error, Result, net};

#[derive(Clone, Debug)]
pub struct ProbeResult {
    pub final_url: Url,
    pub length: Option<u64>,
    pub accepts_ranges: bool,
    pub filename: String,
    /// ETag, else Last-Modified; used to detect that the remote file changed between sessions.
    pub validator: Option<String>,
}

/// `GET` with `Range: bytes=0-0` (works on servers that reject HEAD). Only the headers are read.
/// `origin` is the URL the credentials in `headers` were given for.
pub async fn run(client: &Client, url: &Url, headers: &[(String, String)], origin: &Url) -> Result<ProbeResult> {
    let resp = net::get(client, url, headers, origin, Some("bytes=0-0".into())).await?;
    let h = resp.headers();
    let text = |name: header::HeaderName| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);

    let (length, accepts_ranges) = match resp.status() {
        StatusCode::PARTIAL_CONTENT => match text(header::CONTENT_RANGE).as_deref().and_then(content_range_total) {
            Some(n) => (Some(n), true),
            None => (None, false),
        },
        StatusCode::OK => (text(header::CONTENT_LENGTH).and_then(|v| v.trim().parse().ok()).filter(|&n| n > 0), false),
        s => return Err(Error::BadResponse(s.as_u16())),
    };
    let final_url = resp.url().clone();
    let filename = match text(header::CONTENT_DISPOSITION).as_deref().and_then(parse_content_disposition) {
        Some(name) => sanitize(&name),
        None => sanitize(last_path_component(&final_url)),
    };
    let validator = text(header::ETAG).or_else(|| text(header::LAST_MODIFIED));
    Ok(ProbeResult { final_url, length, accepts_ranges, filename, validator })
}

/// `bytes 0-0/1234` -> 1234.
pub fn content_range_total(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok()
}

fn last_path_component(u: &Url) -> &str {
    let last = u.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
    if last.is_empty() { "download" } else { last }
}

/// RFC 6266: prefers `filename*=UTF-8''...` over `filename="..."`.
pub fn parse_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        let lower = part.to_ascii_lowercase();
        if lower.starts_with("filename*=") {
            let v = &part["filename*=".len()..];
            if let Some(i) = v.find("''")
                && let Ok(dec) = percent_decode_str(&v[i + 2..]).decode_utf8()
            {
                return Some(dec.into_owned());
            }
        } else if lower.starts_with("filename=") {
            let mut v = &part["filename=".len()..];
            if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
                v = &v[1..v.len() - 1];
            }
            plain = Some(v.to_owned());
        }
    }
    plain
}

/// Unicode format characters (category Cf): invisible, and some (U+202E right-to-left override)
/// can disguise a file's real extension.
fn is_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB
        | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Removes control (Cc) and format (Cf) characters.
pub fn strip_invisible(s: &str) -> String {
    s.chars().filter(|&c| !c.is_control() && !is_format(c)).collect()
}

/// Longest name we produce, in bytes. Linux allows 255, and the hidden track files add
/// a leading dot plus suffixes such as `.video.mp4.hoardini`.
const MAX_NAME_BYTES: usize = 200;

pub fn sanitize(name: &str) -> String {
    let decoded = percent_decode_str(name).decode_utf8().map(|c| c.into_owned()).unwrap_or_else(|_| name.to_owned());
    let cleaned = strip_invisible(&decoded).replace(['/', ':'], "_");
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() {
        return "download".to_owned();
    }
    truncate_name(cleaned, MAX_NAME_BYTES)
}

/// Shortens the stem so the whole name fits in `max` bytes, keeping a short extension.
fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, ext) = match split_ext(name) {
        (s, Some(e)) if e.len() <= 16 => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    let mut end = max.saturating_sub(ext.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", stem[..end].trim_end())
}

/// `"a.tar.gz"` -> `("a.tar", Some("gz"))`, `"README"` -> `("README", None)`.
/// A leading dot alone is not an extension.
pub fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    }
}

/// The extension of a URL path or file name, or "".
pub fn path_extension(path: &str) -> &str {
    let last = path.rsplit('/').next().unwrap_or("");
    split_ext(last).1.unwrap_or("")
}

/// A caller-supplied name (e.g. a page title) keeps the real file's extension unless it already has it.
pub fn final_name(over: Option<&str>, probed: &str) -> String {
    let Some(o) = over.map(sanitize) else { return probed.to_owned() };
    match split_ext(probed).1 {
        Some(ext) if !o.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase())) => format!("{o}.{ext}"),
        _ => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_prefers_extended() {
        assert_eq!(
            parse_content_disposition(r#"attachment; filename="plain.txt"; filename*=UTF-8''na%C3%AFve%20file.txt"#).as_deref(),
            Some("naïve file.txt")
        );
        assert_eq!(parse_content_disposition(r#"attachment; filename="report.pdf""#).as_deref(), Some("report.pdf"));
        assert_eq!(parse_content_disposition("inline").as_deref(), None);
    }

    #[test]
    fn sanitize_rules() {
        assert_eq!(sanitize("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize("...hidden"), "hidden");
        assert_eq!(sanitize("a%20b.zip"), "a b.zip");
        assert_eq!(sanitize("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(sanitize("tab\tnew\nline"), "tabnewline");
        assert_eq!(sanitize("   "), "download");
        assert_eq!(sanitize("c:d"), "c_d");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.mp4", "é".repeat(300));
        let s = sanitize(&long);
        assert!(s.len() <= MAX_NAME_BYTES);
        assert!(s.ends_with("é.mp4"));
    }

    #[test]
    fn final_name_keeps_real_extension() {
        assert_eq!(final_name(Some("My Video"), "abc.mp4"), "My Video.mp4");
        assert_eq!(final_name(Some("My Video.MP4"), "abc.mp4"), "My Video.MP4");
        assert_eq!(final_name(Some("Lesson 1.5"), "abc.zip"), "Lesson 1.5.zip");
        assert_eq!(final_name(None, "abc.mp4"), "abc.mp4");
        assert_eq!(final_name(Some("x"), "noext"), "x");
    }

    #[test]
    fn extensions() {
        assert_eq!(split_ext(".bashrc"), (".bashrc", None));
        assert_eq!(split_ext("a."), ("a.", None));
        assert_eq!(path_extension("/v/master.M3U8"), "M3U8");
        assert_eq!(path_extension("/v/dir.x/file"), "");
        assert_eq!(content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(content_range_total("bytes 0-0/*"), None);
    }
}
