//! Firefox native-messaging host. Protocol: 4-byte native-endian length + UTF-8 JSON, on stdin/stdout.
//! The host is short-lived (Firefox ends it when the port closes), so it only validates the
//! request and passes it on:
//!   1. hoardini-app installed next to it -> write the job to its inbox (launching the app if needed).
//!   2. otherwise                         -> start a detached `hoardini` command-line process.
//!
//! Request:  {"url", "referrer", "cookies": "a=b; c=d", "userAgent", "kind": "file"|"hls", "filename", "variant", "audio"}
//! Response: {"ok": true} or {"ok": false, "error": "..."}

use std::{
    io::{self, Read, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

use hoardini_core::{JobRequest, paths};
use serde_json::{Value, json};

fn read_message(stdin: &mut impl Read) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    stdin.read_exact(&mut len).ok()?;
    let len = u32::from_ne_bytes(len) as usize;
    if len == 0 || len >= 1_048_576 {
        return None;
    }
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body).ok()?;
    Some(body)
}

fn send(stdout: &mut impl Write, v: &Value) -> io::Result<()> {
    let data = serde_json::to_vec(v)?;
    stdout.write_all(&(data.len() as u32).to_ne_bytes())?;
    stdout.write_all(&data)?;
    stdout.flush()
}

/// Its own session, so it outlives the host when Firefox closes the port and ends the host's process group.
fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
}

fn deliver_to_app(job: &JobRequest, app: &Path) -> Result<(), String> {
    let queue = || -> io::Result<()> {
        let inbox = paths::inbox();
        paths::private_dir(&inbox)?;
        // Written under a name the app ignores, then renamed: the app never reads half a job.
        let name = uuid::Uuid::new_v4().to_string();
        let data = serde_json::to_vec(job)?;
        paths::write_atomic(&inbox.join(format!("{name}.json")), &data, 0o600)
    };
    queue().map_err(|e| format!("could not queue job: {e}"))?;

    if !paths::app_running() {
        // --background: open in the tray without taking focus from the browser.
        spawn_detached(Command::new(app).arg("--background").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()))
            .map_err(|e| format!("could not launch Hoardini: {e}"))?;
    }
    Ok(())
}

fn run_cli(job: &JobRequest, url: &str) -> Result<(), String> {
    let exe = paths::sibling(paths::CLI_BIN).ok_or("hoardini not found next to the host")?;
    // Headers (cookies included) go through a pipe: command-line arguments are visible to every user via `ps`.
    let mut cmd = Command::new(exe);
    cmd.arg(url).arg("-o").arg(paths::downloads()).arg("--headers-stdin");
    if job.is_hls() {
        cmd.arg("--hls");
        if let Some(v) = job.valid_variant() {
            cmd.arg("--variant").arg(v.as_str());
        }
    }
    if let Some(a) = job.valid_audio() {
        cmd.arg("--audio").arg(a.as_str());
    }
    if let Some(n) = job.clean_filename() {
        cmd.arg("--name").arg(n);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = spawn_detached(&mut cmd).map_err(|e| e.to_string())?;
    let header_text: String = job.headers().iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(header_text.as_bytes());
    }
    Ok(())
}

fn handle(data: &[u8]) -> Value {
    let Some((job, url)) = serde_json::from_slice::<JobRequest>(data).ok().and_then(|j| j.valid_url().map(|u| (j, u))) else {
        return json!({"ok": false, "error": "invalid url"});
    };
    let result = match paths::sibling(paths::APP_BIN) {
        Some(app) => deliver_to_app(&job, &app),
        None => run_cli(&job, url.as_str()),
    };
    match result {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn main() {
    // Firefox passes the manifest path and the extension ID as arguments; nothing else is needed from them.
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    while let Some(msg) = read_message(&mut stdin) {
        if send(&mut stdout, &handle(&msg)).is_err() {
            break;
        }
    }
}
