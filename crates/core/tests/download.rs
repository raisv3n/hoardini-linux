mod common;

use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use common::{Route, Server, data};
use hoardini_core::{Downloader, Error, HlsDownloader, PairDownloader, ProgressFn, remux};

fn quiet() -> ProgressFn {
    Arc::new(|_| {})
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part") || n.ends_with(".hoardini") || n.ends_with(".lock") || n.starts_with('.'))
        .collect()
}

#[tokio::test]
async fn segmented_download() {
    let body = data(3_000_000, 1);
    let srv = Server::start(vec![("/files/big.bin", Route::file(body.clone()))]).await;
    let dir = tempfile::tempdir().unwrap();
    let out = Downloader::new(srv.url("/files/big.bin"), dir.path(), vec![], None).run(quiet()).await.unwrap();
    assert_eq!(out, dir.path().join("big.bin"));
    assert_eq!(fs::read(&out).unwrap(), body);
    // One probe plus one request per segment.
    assert_eq!(srv.requests("/files/big.bin").len(), 1 + 8);
    assert!(leftovers(dir.path()).is_empty(), "{:?}", leftovers(dir.path()));
}

#[tokio::test]
async fn single_stream_without_ranges_and_name_override() {
    let body = data(500_000, 2);
    let srv = Server::start(vec![("/get", Route::no_ranges(body.clone()))]).await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Report"), b"someone else's").unwrap();
    let out = Downloader::new(srv.url("/get"), dir.path(), vec![], Some("Report".into())).run(quiet()).await.unwrap();
    assert_eq!(out.file_name().unwrap(), "Report (1)", "a finished file is never overwritten");
    assert_eq!(fs::read(&out).unwrap(), body);
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn pause_and_resume_continues_where_it_stopped() {
    let body = data(4_000_000, 3);
    let srv = Server::start(vec![("/v.bin", Route::file(body.clone()))]).await;
    *srv.delay.lock().unwrap() = Duration::from_millis(15);
    let dir = tempfile::tempdir().unwrap();

    let seen = Arc::new(AtomicU64::new(0));
    let s2 = seen.clone();
    let progress: ProgressFn = Arc::new(move |p| s2.store(p.done, Ordering::Relaxed));
    let d = Downloader::new(srv.url("/v.bin"), dir.path().to_path_buf(), vec![], None);
    let task = tokio::spawn(async move { d.run(progress).await });
    while seen.load(Ordering::Relaxed) < 1_000_000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let state: serde_json::Value = serde_json::from_slice(&fs::read(dir.path().join("v.bin.hoardini")).unwrap()).unwrap();
    let written: u64 = state["segments"].as_array().unwrap().iter().map(|s| s["written"].as_u64().unwrap()).sum();
    assert!(written >= 1_000_000, "resume state saved on pause ({written})");
    assert!(!dir.path().join("v.bin.lock").exists(), "lock released on pause");

    *srv.delay.lock().unwrap() = Duration::ZERO;
    let before = srv.requests("/v.bin").len();
    let out = Downloader::new(srv.url("/v.bin"), dir.path(), vec![], None).run(quiet()).await.unwrap();
    assert_eq!(out.file_name().unwrap(), "v.bin");
    assert_eq!(fs::read(&out).unwrap(), body);
    let resumed_ranges: Vec<u64> = srv.requests("/v.bin")[before + 1..]
        .iter()
        .map(|r| r.headers["range"].trim_start_matches("bytes=").split('-').next().unwrap().parse().unwrap())
        .collect();
    let starts: Vec<u64> = (0..8).map(|i| i * 500_000).collect();
    assert!(resumed_ranges.iter().any(|r| !starts.contains(r)), "continued mid-segment: {resumed_ranges:?}");
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn changed_file_starts_over() {
    let srv = Server::start(vec![("/f.bin", Route::file(data(2_000_000, 4)))]).await;
    *srv.delay.lock().unwrap() = Duration::from_millis(10);
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(AtomicU64::new(0));
    let s2 = seen.clone();
    let d = Downloader::new(srv.url("/f.bin"), dir.path().to_path_buf(), vec![], None);
    let task = tokio::spawn(async move { d.run(Arc::new(move |p| s2.store(p.done, Ordering::Relaxed))).await });
    while seen.load(Ordering::Relaxed) < 300_000 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;

    let new_body = data(2_000_000, 5);
    let mut r = Route::file(new_body.clone());
    if let Route::Body { etag, .. } = &mut r {
        *etag = Some("\"v2\"".into());
    }
    srv.set("/f.bin", r);
    *srv.delay.lock().unwrap() = Duration::ZERO;
    let out = Downloader::new(srv.url("/f.bin"), dir.path(), vec![], None).run(quiet()).await.unwrap();
    assert_eq!(fs::read(out).unwrap(), new_body, "no bytes of the old version");
}

#[tokio::test]
async fn cookies_stay_on_their_host_across_redirects() {
    let body = data(600_000, 6);
    let srv = Server::start(vec![("/f.bin", Route::file(body.clone()))]).await;
    // Same server, different host name.
    let other = srv.base.replace("127.0.0.1", "localhost");
    srv.set("/go", Route::Redirect(format!("{other}/f.bin")));
    let dir = tempfile::tempdir().unwrap();
    let headers = vec![("Cookie".to_owned(), "session=secret".to_owned()), ("Referer".to_owned(), "https://site.example/".to_owned())];
    let out = Downloader::new(srv.url("/go"), dir.path(), headers, None).run(quiet()).await.unwrap();
    assert_eq!(fs::read(out).unwrap(), body);
    assert!(srv.requests("/go").iter().all(|r| r.headers.get("cookie").map(String::as_str) == Some("session=secret")));
    let cdn = srv.requests("/f.bin");
    assert!(!cdn.is_empty());
    assert!(cdn.iter().all(|r| !r.headers.contains_key("cookie")), "cookie leaked to another host");
    assert!(cdn.iter().all(|r| r.headers.contains_key("referer")));
}

#[tokio::test]
async fn cookies_follow_same_host_redirects() {
    let body = data(300_000, 7);
    let srv = Server::start(vec![("/f.bin", Route::file(body.clone())), ("/go", Route::Redirect("/f.bin".into()))]).await;
    let dir = tempfile::tempdir().unwrap();
    let headers = vec![("Cookie".to_owned(), "session=secret".to_owned())];
    let out = Downloader::new(srv.url("/go"), dir.path(), headers, None).run(quiet()).await.unwrap();
    assert_eq!(fs::read(out).unwrap(), body);
    let reqs = srv.requests("/f.bin");
    assert!(!reqs.is_empty());
    assert!(reqs.iter().all(|r| r.headers.get("cookie").map(String::as_str) == Some("session=secret")));
}

#[tokio::test]
async fn redirect_loop_fails() {
    let srv = Server::start(vec![("/a", Route::Redirect("/b".into())), ("/b", Route::Redirect("/a".into()))]).await;
    let dir = tempfile::tempdir().unwrap();
    let err = Downloader::new(srv.url("/a"), dir.path(), vec![], None).run(quiet()).await.unwrap_err();
    assert!(matches!(err, Error::TooManyRedirects), "{err:?}");
    assert_eq!(srv.requests("/a").len() + srv.requests("/b").len(), hoardini_core::net::MAX_REDIRECTS + 1);
}

fn fmp4_segment(seq: u32, payload: u8) -> Vec<u8> {
    let bx = |kind: &[u8; 4], body: &[u8]| -> Vec<u8> { [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat() };
    let mfhd = bx(b"mfhd", &[&[0, 0, 0, 0][..], &seq.to_be_bytes()].concat());
    let moof = bx(b"moof", &[mfhd, bx(b"traf", &[1; 8])].concat());
    [moof, bx(b"mdat", &[payload; 64])].concat()
}

#[tokio::test]
async fn hls_fmp4_segments_are_renumbered() {
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MAP:URI=\"init.mp4\"\n\
        #EXTINF:6,\ns1.cmfv\n#EXTINF:6,\ns2.cmfv\n#EXTINF:6,\ns3.cmfv\n#EXT-X-ENDLIST\n";
    let init = b"\x00\x00\x00\x10ftypiso6\x00\x00\x00\x00".to_vec();
    let srv = Server::start(vec![
        ("/t/manifest.m3u8", Route::text(playlist)),
        ("/t/init.mp4", Route::file(init.clone())),
        ("/t/s1.cmfv", Route::file(fmp4_segment(1, 1))),
        ("/t/s2.cmfv", Route::file(fmp4_segment(1, 2))),
        ("/t/s3.cmfv", Route::file(fmp4_segment(1, 3))),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = HlsDownloader::new(srv.url("/t/manifest.m3u8?sig=x"), dir.path(), vec![], Some("Lesson 1.5 intro".into()), None)
        .run(quiet())
        .await
        .unwrap();
    assert_eq!(out.file_name().unwrap(), "Lesson 1.5 intro.mp4");
    let expected = [init, fmp4_segment(1, 1), fmp4_segment(2, 2), fmp4_segment(3, 3)].concat();
    assert_eq!(fs::read(out).unwrap(), expected);
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn hls_master_picks_variant_and_byteranges() {
    let all = data(3000, 7);
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900\nhigh.m3u8\n";
    let high = "#EXTM3U\n#EXTINF:2,\n#EXT-X-BYTERANGE:1000@0\nall.ts\n#EXTINF:2,\n#EXT-X-BYTERANGE:2000\nall.ts\n#EXT-X-ENDLIST\n";
    let srv = Server::start(vec![
        ("/m/master.m3u8", Route::text(master)),
        ("/m/low.m3u8", Route::text("#EXTM3U\n#EXTINF:2,\nlow.ts\n#EXT-X-ENDLIST\n")),
        ("/m/high.m3u8", Route::text(high)),
        ("/m/all.ts", Route::file(all.clone())),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = HlsDownloader::new(srv.url("/m/master.m3u8"), dir.path(), vec![], None, None).run(quiet()).await.unwrap();
    assert!(out.file_name().unwrap().to_string_lossy().starts_with("video-"), "{out:?}");
    assert!(out.extension().unwrap() == "ts");
    assert_eq!(fs::read(out).unwrap(), all);
    assert!(srv.requests("/m/low.m3u8").is_empty());
}

#[tokio::test]
async fn hls_byteranges_from_a_server_without_range_support() {
    let all = data(300_000, 8);
    let playlist = "#EXTM3U\n#EXTINF:2,\n#EXT-X-BYTERANGE:100000@0\nall.ts\n#EXTINF:2,\n#EXT-X-BYTERANGE:150000\nall.ts\n\
        #EXTINF:2,\n#EXT-X-BYTERANGE:50000\nall.ts\n#EXT-X-ENDLIST\n";
    let srv = Server::start(vec![("/r/p.m3u8", Route::text(playlist)), ("/r/all.ts", Route::no_ranges(all.clone()))]).await;
    let dir = tempfile::tempdir().unwrap();
    let out = HlsDownloader::new(srv.url("/r/p.m3u8"), dir.path(), vec![], Some("r".into()), None).run(quiet()).await.unwrap();
    assert_eq!(fs::read(out).unwrap(), all, "each segment is cut out of the whole file");
}

#[tokio::test]
async fn hls_limits_what_it_holds_in_memory() {
    let big = format!("#EXTM3U\n{}", "#EXT-X-PLACEHOLDER\n".repeat(1_000_000)); // > 16 MB
    let huge_range = "#EXTM3U\n#EXTINF:2,\n#EXT-X-BYTERANGE:600000000@0\nall.ts\n#EXT-X-ENDLIST\n";
    let srv = Server::start(vec![
        ("/big.m3u8", Route::text(&big)),
        ("/huge.m3u8", Route::text(huge_range)),
        ("/all.ts", Route::file(data(1000, 9))),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let r = HlsDownloader::new(srv.url("/big.m3u8"), dir.path(), vec![], None, None).run(quiet()).await;
    assert!(matches!(r, Err(Error::BadPlaylist)), "{r:?}");
    let r = HlsDownloader::new(srv.url("/huge.m3u8"), dir.path(), vec![], None, None).run(quiet()).await;
    assert!(matches!(r, Err(Error::TooLarge)), "{r:?}");
    assert!(srv.requests("/all.ts").is_empty(), "an oversized segment is refused before downloading");
}

#[tokio::test]
async fn hls_rejects_encrypted_and_live() {
    let srv = Server::start(vec![
        ("/e.m3u8", Route::text("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n")),
        ("/l.m3u8", Route::text("#EXTM3U\n#EXTINF:2,\na.ts\n")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let e = HlsDownloader::new(srv.url("/e.m3u8"), dir.path(), vec![], None, None).run(quiet()).await;
    assert!(matches!(e, Err(Error::EncryptedStream)));
    let l = HlsDownloader::new(srv.url("/l.m3u8"), dir.path(), vec![], None, None).run(quiet()).await;
    assert!(matches!(l, Err(Error::LiveStream)));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "nothing written");
}

#[tokio::test]
async fn hls_pause_and_resume() {
    let mut playlist = String::from("#EXTM3U\n");
    let mut routes = vec![];
    let mut expected = vec![];
    for i in 0..30 {
        playlist += &format!("#EXTINF:2,\nseg{i}.ts\n");
        let d = data(40_000, 100 + i);
        expected.extend_from_slice(&d);
        routes.push((format!("/h/seg{i}.ts"), Route::file(d)));
    }
    playlist += "#EXT-X-ENDLIST\n";
    let srv = Server::start(vec![("/h/index.m3u8", Route::text(&playlist))]).await;
    for (p, r) in routes {
        srv.set(&p, r);
    }
    *srv.delay.lock().unwrap() = Duration::from_millis(20);
    let dir = tempfile::tempdir().unwrap();

    let frac = Arc::new(AtomicU64::new(0));
    let f2 = frac.clone();
    let d = HlsDownloader::new(srv.url("/h/index.m3u8"), dir.path().to_path_buf(), vec![], Some("clip".into()), None);
    let task = tokio::spawn(async move {
        d.run(Arc::new(move |p| f2.store((p.fraction().unwrap_or(0.0) * 1000.0) as u64, Ordering::Relaxed))).await
    });
    while frac.load(Ordering::Relaxed) < 400 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    let state: serde_json::Value = serde_json::from_slice(&fs::read(dir.path().join("clip.ts.hoardini")).unwrap()).unwrap();
    let done = state["tracks"][0]["done"].as_u64().unwrap();
    assert!((12..30).contains(&done), "done = {done}");

    *srv.delay.lock().unwrap() = Duration::ZERO;
    let first = Arc::new(AtomicU64::new(u64::MAX));
    let f3 = first.clone();
    let out = HlsDownloader::new(srv.url("/h/index.m3u8"), dir.path(), vec![], Some("clip".into()), None)
        .run(Arc::new(move |p| {
            let _ = f3.compare_exchange(u64::MAX, p.done, Ordering::Relaxed, Ordering::Relaxed);
        }))
        .await
        .unwrap();
    assert_eq!(out.file_name().unwrap(), "clip.ts");
    assert_eq!(fs::read(out).unwrap(), expected);
    assert_eq!(first.load(Ordering::Relaxed), done * 40_000, "progress starts from what is on disk");
    assert_eq!(srv.requests("/h/seg0.ts").len(), 1, "finished segments are not fetched again");
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn pair_needs_ffmpeg_or_merges() {
    let srv = Server::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    if remux::ffmpeg_path().is_some() {
        return; // covered by tests/remux.rs
    }
    let r = PairDownloader::new(srv.url("/v.mp4"), srv.url("/a.mp4"), dir.path(), vec![], None).run(quiet()).await;
    assert!(matches!(r, Err(Error::FfmpegMissing)));
}
