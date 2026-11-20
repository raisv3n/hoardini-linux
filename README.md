<p align="center">
  <img src="packaging/icons/hoardini-256.png" width="96" alt="Hoardini icon">
</p>

<h1 align="center">Hoardini for Linux</h1>

<p align="center">
  A fast, free download manager for Linux, with a Firefox extension that hands videos and links over to it.
</p>

## What it does

- **Faster downloads.** Each file is split into up to 8 parts that download at the same time.
- **Pause and resume.** Pause a download, quit the app or lose your connection: it continues where it stopped.
  This works for streamed videos too.
- **Save videos.** Detects videos playing on a web page (including HLS `.m3u8` streams such as Vimeo and Tella,
  and Facebook videos in your choice of quality) and saves them as one MP4 without any loss of quality.
- **Works with Firefox.** With the extension installed, Firefox hands video downloads to Hoardini (other files,
  such as `.deb` or `.zip`, stay in Firefox), and links,
  videos and images get a **Download with Hoardini** right-click option. Cookies come from the right
  container if you use Multi-Account Containers.
- **Stays out of the way.** Closing the window keeps downloads running in the tray.

It's free and open source, needs no account, and everything runs on your computer. Hoardini does not download
DRM-protected streams, live streams or YouTube.

## Install

**Ubuntu, Debian and derivatives:** download `hoardini_<version>-1_amd64.deb` from the
[latest release](../../releases/latest) and install it:

```sh
sudo apt install ./hoardini_*_amd64.deb
```

**Any other distribution:** download `Hoardini-linux-x86_64.tar.gz`, extract it and run `./install.sh`
inside. It installs for your account only (`~/.local/bin`), no root needed. `./install.sh --uninstall`
removes it again.

**ffmpeg** is needed to convert videos to MP4 and to save videos whose sound comes separately
(Vimeo, Facebook HD): `sudo apt install ffmpeg`. Hoardini shows a notice until it's installed, and anything
waiting for it continues by itself once it is.

### Add the Firefox extension

Open **Hoardini** from the app menu. The first time, it shows how to add the extension (you can show
this again from **Options ▸ Set Up Firefox Extension…** or the tray icon):

- **Release builds** include the extension signed by Mozilla. Click **Install in Firefox**, then
  **Add** in Firefox.
- **Builds without a signed extension** (for example your own build from source): Firefox only keeps
  add-ons signed by Mozilla, so load it as a temporary add-on, which lasts until Firefox restarts.
  Open `about:debugging`, click **This Firefox ▸ Load Temporary Add-on…** and choose `manifest.json` in
  `~/.local/share/hoardini/extension`. To get a signed one, see [Signing the extension](#signing-the-extension).

Hoardini connects itself to Firefox every time it starts (the native-messaging manifest in
`~/.mozilla/native-messaging-hosts`), so moving or updating it never breaks the connection.
Firefox from the Ubuntu or Mozilla `.deb` packages works; Firefox as a Flatpak can't start programs outside
its sandbox, so it can't reach Hoardini.

## Using Hoardini

| To… | Do this |
|---|---|
| Download a file | Right-click the link ▸ **Download with Hoardini**, or click **Add URL** in the app (Ctrl+N) and paste it. Video downloads in Firefox go to Hoardini by themselves; other files stay in Firefox. |
| Save a video from a web page | Play the video, then click the Hoardini toolbar icon. Pick a quality, adjust the file name, and click **Download**. |
| Download a specific link or image | Right-click it ▸ **Download with Hoardini**. |
| Save a Facebook video | Open the video and click the Hoardini toolbar icon. Pick a quality (720p, 360p… or the HD/SD file) and click **Download**. If the popup asks you to, reload the page first. |
| Pause, resume, retry, cancel | Use the buttons on each download. Quitting (or logging out) pauses running downloads; resume them next time. |
| Open a finished file | Double-click it, or use its open and folder buttons. Files go to your **Downloads** folder. |
| Convert videos to MP4 | MPEG-TS videos (`.ts`, `.mts`, `.m2ts`) are converted automatically after downloading (**Options**). For files you already have, click **Convert Files…**. |
| Let Firefox handle video downloads again | Click the Hoardini toolbar icon and turn off **Capture browser downloads**. |
| Quit completely | Tray icon ▸ **Quit Hoardini**, or Ctrl+Q. Closing the window only hides it. |

### Command line

```
hoardini <url> [-o dir] [-n connections] [-H "Name: value"]... [--headers-stdin] [--name file] [--hls] [--variant url] [--audio url]
hoardini convert <file.ts>...
```

Ctrl+C pauses; running the same command again resumes, for files and HLS streams alike.
`--audio <url>`: `<url>` is a video-only file, and this audio-only file is downloaded alongside it and
merged into one MP4. `--headers-stdin` reads `Name: value` lines from standard input, so cookies never
show up in the process list.

## Privacy

Hoardini runs entirely on your computer, with no accounts, analytics or telemetry. To download files that
need you to be signed in, the extension passes that site's cookies to Hoardini. They're only sent back to
that same site, and they're removed from the download list once the download finishes. Finished files are
tagged with the address they came from (`user.xdg.origin.url`), as Firefox and Chrome do.

---

## For developers

Hoardini for Linux is a Rust port of [Hoardini for macOS](https://github.com/raisv3n/Hoardini) (Swift),
with the Chrome extension ported to Firefox. The download behaviour, file names and resume files
(`<name>.part`, `<name>.hoardini`, hidden `.<name>.video.*` / `.<name>.audio.*` tracks) are the same as
on macOS; see the macOS repository's `SPEC.md` and `docs/port-notes`.

### Building

Requirements: [Rust](https://rustup.rs) 1.88 or later. No system libraries are needed to build; the window
loads X11/Wayland and OpenGL at run time, and the tray and file picker talk to the desktop over D-Bus.

```sh
cargo test --workspace     # unit and integration tests (local HTTP server, no network)
cargo build --release
./install.sh               # install the build for your account and start it
./package.sh               # .deb (needs cargo-deb) and tarball in dist/
```

While working on the extension, load `extension/manifest.json` from this repository as a temporary
add-on in `about:debugging`.

### Signing the extension

Firefox keeps only add-ons signed by Mozilla. Signing is free and doesn't publish anything: the add-on is
submitted as **unlisted** ("On your own"), reviewed automatically, and signed.

- **By hand:** `./scripts/build-xpi.sh`, then upload `dist/hoardini-unsigned.xpi` at
  [addons.mozilla.org ▸ Submit a New Add-on](https://addons.mozilla.org/developers/addon/submit/distribution),
  choose **On your own**, download the signed file and save it as `dist/hoardini.xpi`.
- **With the API:** create [API credentials](https://addons.mozilla.org/developers/addon/api/key/) and run
  `AMO_JWT_ISSUER=… AMO_JWT_SECRET=… ./scripts/sign-xpi.sh` (needs Node.js). In GitHub Actions, add them
  as the repository secrets `AMO_JWT_ISSUER` and `AMO_JWT_SECRET` and the release workflow signs it.

Each upload needs a new `version` in `extension/manifest.json`. `package.sh` includes `dist/hoardini.xpi`
in the `.deb` and the tarball when it's there.

### Releasing

Bump `version` in the root `Cargo.toml` and push a matching tag (`git tag v0.5.10 && git push origin v0.5.10`).
The [release workflow](.github/workflows/release.yml) tests, packages and publishes the `.deb`, the tarball,
the signed extension (when the AMO secrets are set) and their SHA-256 checksums.

### Layout

| Path | macOS original | What it is |
|---|---|---|
| `crates/core` | `HoardiniCore` | probe, segment planner, downloader, HLS, fMP4 fix-up, video+audio pairs, remux |
| `crates/cli` | `hoardini` | the `hoardini` command |
| `crates/host` | `hoardini-host` | Firefox native-messaging host |
| `crates/app` | `HoardiniApp` | egui window, tray icon, download queue, Firefox setup |
| `extension/` | `extension/` | the extension, ported from Chrome to Firefox |

| macOS | Linux |
|---|---|
| `URLSession` + redirect guard | `reqwest` with rustls (drops Cookie/Authorization on cross-host redirects) |
| AVFoundation passthrough export | `ffmpeg -c copy` |
| Quarantine attribute | `user.xdg.origin.url` / `user.xdg.referrer.url` extended attributes |
| `flock` on `<name>.lock` | the same |
| SwiftUI window + `MenuBarExtra` | egui window + StatusNotifierItem tray icon |
| `~/Library/Application Support/Hoardini` | `~/.local/share/hoardini` |
| `open -g -b com.hoardini.app` | `flock` on `app.lock` + `hoardini-app --background` |
| Chrome `NativeMessagingHosts` + extension key | `~/.mozilla/native-messaging-hosts` + Gecko ID `hoardini@raisv3n` |

Firefox-specific changes in the extension: `browser.*` APIs and an event page instead of a service worker;
the requesting document's origin comes from `documentUrl` (Chrome's `initiator`); cookies are read from the
tab's container and work with first-party isolation; Hoardini's own fallback downloads are never captured
again; and the popup asks for site access when Firefox hasn't granted it.

## License

MIT. See [LICENSE](LICENSE).
