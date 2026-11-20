//! usage: hoardini <url> [-o dir] [-n connections] [-H "Name: value"]... [--headers-stdin] [--name file] [--hls] [--variant url] [--audio url]
//!        hoardini convert <file.ts>...
//!
//! --audio: <url> is a video-only file; download this audio-only file too and merge both into one MP4.
//! --headers-stdin reads more "Name: value" lines from stdin, so cookies never appear in the process list.
//! Ctrl+C pauses; running the same command again resumes.

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hoardini_core::{DownloadProgress, Downloader, HlsDownloader, PairDownloader, ProgressFn, job::http_url, probe, remux};
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "usage: hoardini <http(s) url> [-o dir] [-n connections] [-H \"Name: value\"]... [--headers-stdin] [--name file] [--hls] [--variant url] [--audio url]\n       hoardini convert <file.ts>...";

fn take(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    if i + 1 >= args.len() {
        return None;
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    match args.iter().position(|a| a == flag) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

fn add_header(headers: &mut Vec<(String, String)>, h: &str) {
    if let Some((k, v)) = h.split_once(':') {
        let (k, v) = (k.trim(), v.trim());
        if !k.is_empty() && !v.chars().any(|c| c.is_control() && c != '\t') {
            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            headers.push((k.to_owned(), v.to_owned()));
        }
    }
}

async fn convert(files: &[String]) -> ExitCode {
    let mut failed = false;
    for f in files {
        match remux::to_mp4(&PathBuf::from(f)).await {
            Ok(out) => println!("{}", out.display()),
            Err(e) => {
                eprintln!("{f}: {e}");
                failed = true;
            }
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("convert") {
        return convert(&args[1..]).await;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version") {
        println!("hoardini {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let dir = take(&mut args, "-o").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let conns = take(&mut args, "-n").and_then(|n| n.parse().ok()).unwrap_or(8usize).clamp(1, 32);
    let mut headers = Vec::new();
    while let Some(h) = take(&mut args, "-H") {
        add_header(&mut headers, &h);
    }
    if take_flag(&mut args, "--headers-stdin") {
        let mut input = String::new();
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
        input.lines().for_each(|l| add_header(&mut headers, l));
    }
    let name = take(&mut args, "--name");
    let variant = take(&mut args, "--variant").and_then(|v| http_url(&v));
    let audio = take(&mut args, "--audio").and_then(|a| http_url(&a));
    let force_hls = take_flag(&mut args, "--hls");

    let (Some(url), 1) = (args.first().and_then(|a| http_url(a)), args.len()) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("error: {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }

    let start = Instant::now();
    let last = Mutex::new(Instant::now() - Duration::from_secs(1));
    let on_progress: ProgressFn = Arc::new(move |p: DownloadProgress| {
        let mut l = last.lock().unwrap();
        let finished = p.total == Some(p.done) || p.fraction_override == Some(1.0);
        if !finished && l.elapsed() < Duration::from_millis(250) {
            return;
        }
        *l = Instant::now();
        let secs = start.elapsed().as_secs_f64().max(0.001);
        let mb = p.done as f64 / 1_048_576.0;
        let pct = p.fraction().map(|f| format!("{:5.1}%", f * 100.0)).unwrap_or_else(|| "  ?  ".into());
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{pct}  {mb:.1} MB  {:.1} MB/s ", mb / secs);
        let _ = err.flush();
    });

    let is_hls = force_hls || probe::path_extension(url.path()).eq_ignore_ascii_case("m3u8");
    let job = async {
        if is_hls {
            HlsDownloader::new(url, &dir, headers, name, variant).run(on_progress).await
        } else if let Some(audio) = audio {
            PairDownloader::new(url, audio, &dir, headers, name).run(on_progress).await
        } else {
            let mut d = Downloader::new(url, &dir, headers, name);
            d.connections = conns;
            d.run(on_progress).await
        }
    };
    let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
        return ExitCode::FAILURE;
    };
    // Stopping drops the download: its resume state is saved and its name lock released.
    let result = tokio::select! {
        r = job => r,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\npaused; run the same command again to resume");
            return ExitCode::from(130);
        }
        _ = term.recv() => return ExitCode::from(143),
        _ = hup.recv() => return ExitCode::from(129),
    };
    match result {
        Ok(out) => {
            eprintln!();
            println!("{}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nerror: {e}");
            ExitCode::FAILURE
        }
    }
}
