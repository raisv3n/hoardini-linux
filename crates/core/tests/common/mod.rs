//! A tiny HTTP/1.1 server for the integration tests: static bodies with optional Range support,
//! redirects, a per-chunk delay to make pausing deterministic, and a log of the requests it saw.

#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Clone)]
pub enum Route {
    Body { data: Arc<Vec<u8>>, ranges: bool, etag: Option<String>, content_type: &'static str },
    Redirect(String),
}

impl Route {
    pub fn file(data: Vec<u8>) -> Self {
        Route::Body { data: Arc::new(data), ranges: true, etag: Some("\"v1\"".into()), content_type: "application/octet-stream" }
    }
    pub fn no_ranges(data: Vec<u8>) -> Self {
        Route::Body { data: Arc::new(data), ranges: false, etag: None, content_type: "application/octet-stream" }
    }
    pub fn text(s: &str) -> Self {
        Route::Body { data: Arc::new(s.as_bytes().to_vec()), ranges: true, etag: None, content_type: "application/vnd.apple.mpegurl" }
    }
}

#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub headers: HashMap<String, String>,
}

#[derive(Clone)]
pub struct Server {
    pub base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    /// Pause after each 16 KiB of body.
    pub delay: Arc<Mutex<Duration>>,
}

impl Server {
    pub async fn start(routes: Vec<(&str, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let s = Server {
            base,
            routes: Arc::new(Mutex::new(routes.into_iter().map(|(p, r)| (p.to_owned(), r)).collect())),
            seen: Default::default(),
            delay: Default::default(),
        };
        let srv = s.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                let srv = srv.clone();
                tokio::spawn(async move {
                    let _ = srv.serve(sock).await;
                });
            }
        });
        s
    }

    pub fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("{}{path}", self.base)).unwrap()
    }

    pub fn set(&self, path: &str, r: Route) {
        self.routes.lock().unwrap().insert(path.to_owned(), r);
    }

    pub fn requests(&self, path: &str) -> Vec<Seen> {
        self.seen.lock().unwrap().iter().filter(|s| s.path == path).cloned().collect()
    }

    async fn serve(&self, mut sock: TcpStream) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let head = String::from_utf8_lossy(&buf).into_owned();
        let mut lines = head.split("\r\n");
        let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("/").to_owned();
        let headers: HashMap<String, String> = lines
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())))
            .collect();
        self.seen.lock().unwrap().push(Seen { path: path.clone(), headers: headers.clone() });
        let route_key = path.split('?').next().unwrap_or("").to_owned();
        let route = self.routes.lock().unwrap().get(&route_key).cloned();
        let Some(route) = route else {
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        };
        match route {
            Route::Redirect(to) => {
                let r = format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                sock.write_all(r.as_bytes()).await?;
            }
            Route::Body { data, ranges, etag, content_type } => {
                let len = data.len() as u64;
                let range = headers.get("range").filter(|_| ranges).and_then(|r| {
                    let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
                    let a: u64 = a.parse().ok()?;
                    let b: u64 = if b.is_empty() { len - 1 } else { b.parse::<u64>().ok()?.min(len - 1) };
                    (a <= b).then_some((a, b))
                });
                let (status, start, end) = match range {
                    Some((a, b)) => ("206 Partial Content", a, b),
                    None => ("200 OK", 0, len.saturating_sub(1)),
                };
                let body = if len == 0 { &data[..0] } else { &data[start as usize..=end as usize] };
                let mut h = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                if range.is_some() {
                    h += &format!("Content-Range: bytes {start}-{end}/{len}\r\n");
                }
                if let Some(e) = etag {
                    h += &format!("ETag: {e}\r\n");
                }
                h += "\r\n";
                sock.write_all(h.as_bytes()).await?;
                let delay = *self.delay.lock().unwrap();
                for chunk in body.chunks(16 * 1024) {
                    sock.write_all(chunk).await?;
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        sock.shutdown().await
    }
}

/// Deterministic pseudo-random bytes.
pub fn data(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}
