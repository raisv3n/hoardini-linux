//! The download queue (DownloadManager.swift): starts up to three downloads at a time, pauses,
//! resumes and cancels them, converts MPEG-TS results to MP4, picks up jobs from the browser
//! host's inbox, and keeps the list in `~/.local/share/hoardini/queue.json`.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use eframe::egui;
use hoardini_core::{
    DestinationFn, DownloadProgress, Downloader, HlsDownloader, JobRequest, PairDownloader, ProgressFn, net, paths, remux,
};
use tokio::{runtime::Handle, task::JoinHandle};
use uuid::Uuid;

use crate::model::{self, DownloadItem, Settings, Status};

const MAX_CONCURRENT: usize = 3;

struct Inner {
    items: Vec<DownloadItem>,
    settings: Settings,
    tasks: HashMap<Uuid, JoinHandle<()>>,
    /// Paused tasks still unwinding; a resume waits for them so it reuses their files instead of starting a new copy.
    stopping: HashMap<Uuid, JoinHandle<()>>,
    last_sample: HashMap<Uuid, (Instant, u64)>,
    ffmpeg: bool,
}

pub struct Manager {
    inner: Mutex<Inner>,
    rt: Handle,
    ctx: OnceLock<egui::Context>,
    downloads_dir: PathBuf,
    /// Set by a second launch or the tray; the window shows itself on its next tick.
    pub show_requested: AtomicBool,
    /// Set on SIGTERM/SIGINT (logging out, shutting down); the window quits on its next tick.
    pub quit_requested: AtomicBool,
}

fn queue_file() -> PathBuf {
    paths::support().join("queue.json")
}

fn settings_file() -> PathBuf {
    paths::support().join("settings.json")
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

impl Manager {
    pub fn new(rt: Handle) -> Arc<Self> {
        let _ = paths::private_dir(&paths::support());
        let _ = paths::private_dir(&paths::inbox());
        let mut items: Vec<DownloadItem> = read_json(&queue_file()).unwrap_or_default();
        for i in &mut items {
            // Anything mid-flight when the app quit is paused, ready to resume where it stopped.
            if i.status == Status::Downloading {
                i.status = Status::Paused;
            }
        }
        let m = Arc::new(Self {
            inner: Mutex::new(Inner {
                items,
                settings: read_json(&settings_file()).unwrap_or_default(),
                tasks: HashMap::new(),
                stopping: HashMap::new(),
                last_sample: HashMap::new(),
                ffmpeg: remux::ffmpeg_path().is_some(),
            }),
            rt,
            ctx: OnceLock::new(),
            downloads_dir: paths::downloads(),
            show_requested: AtomicBool::new(false),
            quit_requested: AtomicBool::new(false),
        });
        let poller = Arc::downgrade(&m);
        std::thread::Builder::new()
            .name("inbox".into())
            .spawn(move || {
                let mut ticks = 0u32;
                while let Some(m) = poller.upgrade() {
                    m.poll_inbox();
                    if ticks.is_multiple_of(20) {
                        m.refresh_ffmpeg();
                    }
                    ticks = ticks.wrapping_add(1);
                    drop(m);
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .expect("inbox thread");
        {
            let mut g = m.lock();
            // Writes the queue back without expired cookies.
            m.save(&mut g);
            m.pump(&mut g);
        }
        m
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_context(&self, ctx: egui::Context) {
        let _ = self.ctx.set(ctx);
    }

    fn repaint(&self) {
        if let Some(c) = self.ctx.get() {
            c.request_repaint();
        }
    }

    // ---------- reading ----------

    pub fn items(&self) -> Vec<DownloadItem> {
        self.lock().items.clone()
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut g = self.lock();
        f(&mut g.settings);
        if let Ok(data) = serde_json::to_vec_pretty(&g.settings) {
            let _ = paths::write_atomic(&settings_file(), &data, 0o600);
        }
    }

    pub fn has_ffmpeg(&self) -> bool {
        self.lock().ffmpeg
    }

    /// (downloading, total speed in bytes/s)
    pub fn activity(&self) -> (usize, f64) {
        let g = self.lock();
        let active: Vec<_> = g.items.iter().filter(|i| i.status == Status::Downloading).collect();
        (active.len(), active.iter().map(|i| i.speed).sum())
    }

    pub fn finished_count(&self) -> usize {
        self.lock().items.iter().filter(|i| i.is_finished()).count()
    }

    // ---------- adding ----------

    pub fn add(self: &Arc<Self>, request: JobRequest) {
        if request.valid_url().is_none() {
            return;
        }
        let mut g = self.lock();
        g.items.insert(0, DownloadItem::new(request));
        self.save(&mut g);
        self.pump(&mut g);
        drop(g);
        self.repaint();
    }

    /// Jobs dropped into the inbox directory by the browser host, and "show" requests from a second launch.
    fn poll_inbox(self: &Arc<Self>) {
        let Ok(entries) = fs::read_dir(paths::inbox()) else { return };
        for e in entries.flatten() {
            let path = e.path();
            match path.extension().and_then(|x| x.to_str()) {
                Some("json") => {
                    let job = (e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) < 1 << 20)
                        .then(|| read_json::<JobRequest>(&path))
                        .flatten();
                    let _ = fs::remove_file(&path);
                    if let Some(job) = job {
                        self.add(job);
                    }
                }
                Some("show") => {
                    let _ = fs::remove_file(&path);
                    self.request_show();
                }
                _ => {}
            }
        }
    }

    pub fn request_show(&self) {
        self.show_requested.store(true, Ordering::SeqCst);
        self.repaint();
    }

    pub fn request_quit(&self) {
        self.quit_requested.store(true, Ordering::SeqCst);
        self.repaint();
    }

    fn refresh_ffmpeg(self: &Arc<Self>) {
        let found = remux::ffmpeg_path().is_some();
        let waiting: Vec<Uuid> = {
            let mut g = self.lock();
            if g.ffmpeg == found {
                return;
            }
            g.ffmpeg = found;
            if !found {
                return;
            }
            g.items.iter_mut().filter(|i| i.needs_ffmpeg).map(|i| {
                i.needs_ffmpeg = false;
                i.id
            }).collect()
        };
        // Installed while the app was running: convert what was waiting for it.
        for id in waiting {
            self.convert(id);
        }
        self.repaint();
    }

    // ---------- controls ----------

    pub fn pause(self: &Arc<Self>, id: Uuid) {
        let mut g = self.lock();
        let Some(i) = index(&g, id) else { return };
        if !g.items[i].can_pause() {
            return;
        }
        g.items[i].status = Status::Paused;
        g.items[i].speed = 0.0;
        if let Some(t) = g.tasks.remove(&id) {
            t.abort();
            g.stopping.insert(id, t);
        }
        self.save(&mut g);
        self.pump(&mut g);
    }

    pub fn resume(self: &Arc<Self>, id: Uuid) {
        let mut g = self.lock();
        let Some(i) = index(&g, id) else { return };
        if !matches!(g.items[i].status, Status::Paused | Status::Failed) {
            return;
        }
        g.items[i].status = Status::Queued;
        g.items[i].error = None;
        self.save(&mut g);
        self.pump(&mut g);
    }

    /// Stops the download and deletes any partial data.
    pub fn cancel_and_remove(self: &Arc<Self>, id: Uuid) {
        let mut g = self.lock();
        let running: Vec<JoinHandle<()>> = [g.tasks.remove(&id), g.stopping.remove(&id)].into_iter().flatten().collect();
        running.iter().for_each(JoinHandle::abort);
        if let Some(i) = index(&g, id) {
            if g.items[i].status != Status::Completed
                && let Some(d) = g.items[i].destination.clone()
            {
                // Delete only once the download has stopped writing.
                self.rt.spawn(async move {
                    for t in running {
                        let _ = t.await;
                    }
                    net::remove_partial(&d);
                });
            }
            g.items.remove(i);
        }
        self.save(&mut g);
        self.pump(&mut g);
    }

    pub fn clear_finished(&self) {
        let mut g = self.lock();
        g.items.retain(|i| !i.is_finished());
        self.save(&mut g);
    }

    /// Removes one entry from the list without touching its files.
    pub fn remove_from_list(&self, id: Uuid) {
        let mut g = self.lock();
        if let Some(i) = index(&g, id).filter(|&i| g.items[i].is_finished()) {
            g.items.remove(i);
            self.save(&mut g);
        }
    }

    /// The file "Open" / "Show in Folder" should act on, if it still exists.
    pub fn playable(&self, id: Uuid) -> Option<PathBuf> {
        let g = self.lock();
        let p = g.items.iter().find(|i| i.id == id)?.playable()?.to_path_buf();
        p.exists().then_some(p)
    }

    // ---------- MP4 conversion ----------

    /// Adds existing files (e.g. earlier downloads) to the list and converts them.
    pub fn convert_files(self: &Arc<Self>, files: Vec<PathBuf>) {
        let mut ids = Vec::new();
        {
            let mut g = self.lock();
            for f in files {
                let url = url::Url::from_file_path(&f).map(String::from).unwrap_or_default();
                let mut item = DownloadItem::new(JobRequest::new(url));
                item.status = Status::Completed;
                item.total = fs::metadata(&f).ok().map(|m| m.len());
                item.done = item.total.unwrap_or(0);
                item.destination = Some(f);
                item.fraction = Some(1.0);
                ids.push(item.id);
                g.items.insert(0, item);
            }
            self.save(&mut g);
        }
        for id in ids {
            self.convert(id);
        }
    }

    pub fn convert(self: &Arc<Self>, id: Uuid) {
        let mut g = self.lock();
        let Some(i) = index(&g, id) else { return };
        if !g.items[i].can_convert() {
            return;
        }
        let Some(src) = g.items[i].destination.clone() else { return };
        if !g.ffmpeg {
            g.items[i].needs_ffmpeg = true;
            return;
        }
        g.items[i].converting = true;
        g.items[i].convert_error = None;
        let delete_original = g.settings.delete_original;
        drop(g);
        self.repaint();
        let m = self.clone();
        self.rt.spawn(async move {
            let result = remux::to_mp4(&src).await;
            let mut g = m.lock();
            if let Some(i) = index(&g, id) {
                g.items[i].converting = false;
                match result {
                    Ok(mp4) => {
                        if delete_original {
                            let _ = fs::remove_file(&src);
                            g.items[i].destination = Some(mp4.clone());
                        }
                        g.items[i].mp4 = Some(mp4);
                    }
                    Err(hoardini_core::Error::FfmpegMissing) => {
                        g.ffmpeg = false;
                        g.items[i].needs_ffmpeg = true;
                    }
                    Err(e) => g.items[i].convert_error = Some(e.to_string()),
                }
                m.save(&mut g);
            }
            drop(g);
            m.repaint();
        });
    }

    /// Quitting pauses everything that was downloading; it resumes from where it stopped next time.
    pub fn shutdown(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut g = self.lock();
            for i in &mut g.items {
                if i.status == Status::Downloading {
                    i.status = Status::Paused;
                    i.speed = 0.0;
                }
            }
            self.save(&mut g);
            let mut hs: Vec<_> = g.tasks.drain().map(|(_, t)| t).collect();
            hs.extend(g.stopping.drain().map(|(_, t)| t));
            hs
        };
        handles.iter().for_each(JoinHandle::abort);
        // Let the aborted downloads write their resume state and release their name locks.
        self.rt.block_on(async {
            let _ = tokio::time::timeout(Duration::from_secs(3), futures_join(handles)).await;
        });
    }

    // ---------- scheduling ----------

    fn pump(self: &Arc<Self>, g: &mut Inner) {
        let running = g.items.iter().filter(|i| i.status == Status::Downloading).count();
        // Oldest first: new items are inserted at the top.
        let queued: Vec<Uuid> = g.items.iter().rev().filter(|i| i.status == Status::Queued).map(|i| i.id).collect();
        for id in queued.into_iter().take(MAX_CONCURRENT.saturating_sub(running)) {
            self.start(g, id);
        }
    }

    fn start(self: &Arc<Self>, g: &mut Inner, id: Uuid) {
        let Some(i) = index(g, id) else { return };
        g.items[i].status = Status::Downloading;
        g.items[i].speed = 0.0;
        g.items[i].error = None;
        g.last_sample.insert(id, (Instant::now(), g.items[i].done));

        let req = g.items[i].request.clone();
        let Some(url) = req.valid_url() else {
            self.fail_locked(g, id, "Invalid URL".into());
            return;
        };
        let dir = self.downloads_dir.clone();
        // On resume keep the name the downloader chose the first time so its state file is found.
        let name = g.items[i].destination.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).or_else(|| req.clean_filename());
        let prior = g.stopping.remove(&id);
        let m = self.clone();

        let task = self.rt.spawn(async move {
            if let Some(p) = prior {
                let _ = p.await;
            }
            let _ = paths::private_dir(&paths::support());
            let _ = fs::create_dir_all(&dir);
            let weak = Arc::downgrade(&m);
            let on_progress: ProgressFn = Arc::new(move |p| {
                if let Some(m) = weak.upgrade() {
                    m.progress(id, p);
                }
            });
            let weak = Arc::downgrade(&m);
            let on_dest: DestinationFn = Arc::new(move |p: &Path| {
                if let Some(m) = weak.upgrade() {
                    let mut g = m.lock();
                    if let Some(i) = index(&g, id) {
                        g.items[i].destination = Some(p.to_path_buf());
                        m.save(&mut g);
                    }
                }
            });
            let headers = req.headers();
            let result = if req.is_hls() {
                let mut d = HlsDownloader::new(url, &dir, headers, name, req.valid_variant());
                d.on_destination = Some(on_dest);
                d.run(on_progress).await
            } else if let Some(audio) = req.valid_audio() {
                let mut d = PairDownloader::new(url, audio, &dir, headers, name);
                d.on_destination = Some(on_dest);
                d.run(on_progress).await
            } else {
                let mut d = Downloader::new(url, &dir, headers, name);
                d.on_destination = Some(on_dest);
                d.run(on_progress).await
            };
            match result {
                Ok(out) => m.finish(id, out),
                Err(e) => m.fail(id, e.to_string()),
            }
        });
        g.tasks.insert(id, task);
    }

    fn progress(&self, id: Uuid, p: DownloadProgress) {
        let mut g = self.lock();
        let Some(i) = index(&g, id) else { return };
        if g.items[i].status != Status::Downloading {
            return;
        }
        let now = Instant::now();
        let item = &mut g.items[i];
        item.done = p.done;
        item.total = p.total;
        item.fraction = p.fraction();
        let prev_speed = item.speed;
        let sample = g.last_sample.get(&id).copied();
        if let Some((t, bytes)) = sample {
            let dt = now.duration_since(t).as_secs_f64();
            if dt >= 0.5 {
                let inst = p.done.saturating_sub(bytes) as f64 / dt;
                g.items[i].speed = if prev_speed == 0.0 { inst } else { prev_speed * 0.7 + inst * 0.3 };
                g.last_sample.insert(id, (now, p.done));
            }
        }
    }

    fn finish(self: &Arc<Self>, id: Uuid, out: PathBuf) {
        let auto = {
            let mut g = self.lock();
            let Some(i) = index(&g, id) else { return };
            let item = &mut g.items[i];
            item.status = Status::Completed;
            item.destination = Some(out);
            item.fraction = Some(1.0);
            if let Some(t) = item.total {
                item.done = t;
            }
            item.speed = 0.0;
            // Session cookies are not kept once they are no longer needed.
            item.request.cookies = None;
            g.tasks.remove(&id);
            self.save(&mut g);
            self.pump(&mut g);
            g.settings.auto_convert
        };
        if auto {
            self.convert(id);
        }
        self.repaint();
    }

    fn fail(self: &Arc<Self>, id: Uuid, message: String) {
        let mut g = self.lock();
        self.fail_locked(&mut g, id, message);
        drop(g);
        self.repaint();
    }

    fn fail_locked(self: &Arc<Self>, g: &mut Inner, id: Uuid, message: String) {
        let Some(i) = index(g, id) else { return };
        g.items[i].status = Status::Failed;
        g.items[i].error = Some(message);
        g.items[i].speed = 0.0;
        g.tasks.remove(&id);
        self.save(g);
        self.pump(g);
    }

    // ---------- persistence ----------

    fn save(&self, g: &mut Inner) {
        model::expire_cookies(&mut g.items, chrono::Utc::now().timestamp());
        if let Ok(data) = serde_json::to_vec(&g.items) {
            let _ = paths::write_atomic(&queue_file(), &data, 0o600);
        }
    }
}

fn index(g: &Inner, id: Uuid) -> Option<usize> {
    g.items.iter().position(|i| i.id == id)
}

async fn futures_join(handles: Vec<JoinHandle<()>>) {
    for h in handles {
        let _ = h.await;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        time::Duration,
    };

    use super::*;

    /// A Range-capable HTTP server that sends 16 KiB every 5 ms, so downloads can be caught mid-way.
    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files: Arc<HashMap<&'static str, Vec<u8>>> = Arc::new(files.into_iter().collect());
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let files = files.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap_or(0);
                    let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
                    let mut range = None;
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("range: bytes=") {
                            let (a, b) = v.trim().split_once('-').unwrap();
                            range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().ok()));
                        }
                    }
                    let mut w = sock;
                    let Some(body) = files.get(path.as_str()) else {
                        let _ = w.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    };
                    let (a, b) = range.map(|(a, b)| (a, b.unwrap_or(body.len() - 1).min(body.len() - 1))).unwrap_or((0, body.len() - 1));
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\nETag: \"1\"\r\nConnection: close\r\n\r\n",
                        b - a + 1,
                        body.len()
                    );
                    if w.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    for chunk in body[a..=b].chunks(16 * 1024) {
                        if w.write_all(chunk).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
            }
        });
        base
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn item(m: &Manager, id: Uuid) -> Option<DownloadItem> {
        m.items().into_iter().find(|i| i.id == id)
    }

    fn body(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u32).wrapping_mul(2654435761).rotate_left(seed as u32) as u8).collect()
    }

    #[test]
    fn queue_pause_resume_cancel_and_quit() {
        let home = tempfile::tempdir().unwrap();
        // The only test that touches the environment; nothing else in this binary reads it.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
            std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        }
        let downloads = home.path().join("Downloads");
        let (a, b, c) = (body(3_000_000, 1), body(3_000_000, 2), body(3_000_000, 3));
        let base = serve(vec![("/a.bin", a.clone()), ("/b.bin", b), ("/c.bin", c.clone())]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let m = Manager::new(rt.handle().clone());
        let only_id = |m: &Manager| m.items()[0].id;

        // Pause, then resume straight away: the same file continues (no "a (1).bin").
        m.add(JobRequest::new(format!("{base}/a.bin")));
        let id = only_id(&m);
        wait_for("a to start", || item(&m, id).is_some_and(|i| i.done > 300_000));
        m.pause(id);
        assert_eq!(item(&m, id).unwrap().status, Status::Paused);
        m.resume(id);
        wait_for("a to finish", || item(&m, id).is_some_and(|i| i.status == Status::Completed));
        assert_eq!(fs::read(downloads.join("a.bin")).unwrap(), a);
        assert!(!downloads.join("a (1).bin").exists());

        // Cancel deletes the partial files once the download has stopped.
        m.add(JobRequest::new(format!("{base}/b.bin")));
        let id = only_id(&m);
        wait_for("b to start", || item(&m, id).is_some_and(|i| i.done > 300_000));
        m.cancel_and_remove(id);
        assert!(item(&m, id).is_none());
        wait_for("b's leftovers to go", || !downloads.join("b.bin.part").exists());
        std::thread::sleep(Duration::from_millis(300));
        for leftover in ["b.bin", "b.bin.part", "b.bin.hoardini", "b.bin.lock"] {
            assert!(!downloads.join(leftover).exists(), "{leftover} left behind");
        }

        // Quitting pauses; the next launch shows it paused and resumes it.
        m.add(JobRequest::new(format!("{base}/c.bin")));
        let id = only_id(&m);
        wait_for("c to start", || item(&m, id).is_some_and(|i| i.done > 300_000));
        m.shutdown();
        assert!(downloads.join("c.bin.hoardini").exists());
        assert!(!downloads.join("c.bin.lock").exists(), "name lock released on quit");
        drop(m);

        let m = Manager::new(rt.handle().clone());
        let c_item = item(&m, id).unwrap();
        assert_eq!(c_item.status, Status::Paused);
        assert!(c_item.done > 300_000);
        m.resume(id);
        wait_for("c to finish", || item(&m, id).is_some_and(|i| i.status == Status::Completed));
        assert_eq!(fs::read(downloads.join("c.bin")).unwrap(), c);
        assert_eq!(m.finished_count(), 2);
        m.clear_finished();
        assert!(m.items().is_empty());
        assert!(downloads.join("c.bin").exists(), "clearing the list keeps the files");
    }
}
