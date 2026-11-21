# KPlay

A small AirPlay screen-mirroring receiver for Windows (and anything Rust + ffmpeg runs on).
Pick it from **Screen Mirroring** on an iPhone/iPad and your screen and sound show up in a window.

Run `kplay` and a small control panel opens: set the device name, port, audio output,
ffmpeg path, start-muted and keep-on-top, then **Save** (the receiver restarts to apply).
Start/Stop, Mute and Reset audio are buttons there too, with a live status and log.
Settings are stored in `%APPDATA%\kplay\config.json`.

For a console-only run: `kplay --headless --name "Living Room PC"` (the name is saved).

## Window controls

| Input                     | Action                                                   |
| ------------------------- | -------------------------------------------------------- |
| **Scroll wheel**          | Resize the window (keeps the picture's aspect ratio, stays centred) |
| **M**                     | Mute / unmute the stream (fades, so no clicks)           |
| **R**                     | Reset audio: restarts the decoder, re-opens the output device, drops buffered sound |
| **F**, **F11**, double-click | Fullscreen (Esc leaves it)                            |
| **T**                     | Keep window on top                                       |
| Drag                      | Move the window                                          |

The window stays hidden until a phone connects and goes away when it disconnects.
It re-fits when you rotate the phone. Mute/reset/pin show a small on-screen indicator.

Use **R** when audio gets stuck, crackles, or after you switch headphones/speakers: the default output device is looked up again on reset.
Audio also self-heals on its own: if the buffer ever drifts too far behind, the oldest sound is dropped, and a crashed decoder is restarted.

## Options

```
    --headless        Run the receiver without the control panel
-n, --name <NAME>     Name shown in the iOS Screen Mirroring list (saved)
-p, --port <PORT>     Control port (default 7000)
    --ffmpeg <PATH>   ffmpeg to use (default: bundled copy, then PATH)
    --muted           Start muted
-v, --verbose         Debug logging
```

Identity (name, device id, long-term key) lives in `%APPDATA%\kplay\config.json`.
Delete it to appear as a brand-new device.

## Requirements

* Rust (stable) and a C compiler (the vendored FairPlay code is C; MinGW gcc or MSVC both work).
* Either use the bundled release, or put `ffmpeg` on `PATH` (`winget install Gyan.FFmpeg`). It decodes H.264 video and AAC-ELD audio.
* iPhone/iPad on the same network. Allow KPlay through the Windows firewall on first run (private networks).

```
cargo run --release -- --name "My PC"
```

GitHub Releases provide two Windows downloads: a small standalone KPlay archive
that uses an existing ffmpeg installation, and a larger `-bundled-ffmpeg` archive
that works without installing ffmpeg separately.

## How it works

```
iPhone ──mDNS──▶ _airplay._tcp / _raop._tcp  (discovery.rs)
       ──TCP───▶ HTTP/RTSP control: /info, pair-verify, fp-setup, SETUP...  (server.rs)
       ──TCP───▶ mirror stream: AES-CTR encrypted H.264 ─▶ ffmpeg ─▶ window  (video.rs, display.rs)
       ──UDP───▶ RTP audio: AES-CBC encrypted AAC-ELD ─▶ ffmpeg ─▶ cpal      (rtp.rs, audio.rs)
```

* Legacy AirPlay (v1-style) mirroring: curve25519/ed25519 pair-verify, FairPlay v3 key exchange, stream keys derived as in UxPlay/RPiPlay.
* ffmpeg runs as a child process for each of video and audio, tuned for latency (no probing delay, single-threaded encoder stage; see comments in `video.rs`). Frames go to the window newest-wins, so a slow window never builds up lag.
* The window is plain `winit` + `softbuffer`; scaling is a small CPU box/bilinear filter (multi-threaded for big windows).

## Not supported (yet)

* AirPlay 2 only features: multi-room, buffered audio / "AirPlay music" (ALAC), HLS video casting. Only mirroring (screen + its sound).
* FairPlay "v2" handshake that very new iOS versions *may* request. The legacy v3 path is what current iOS still uses for mirroring.
* H.265 (4K) mirroring.
* Audio/video are shown as they arrive (no clock sync), which keeps latency minimal but means lip-sync is approximate.

## Licence / credits

The FairPlay key-exchange code in `vendor/` and the protocol details come from the
[UxPlay](https://github.com/FDH2/UxPlay) / RPiPlay projects (GPL-3.0 / LGPL-2.1+), so KPlay is distributed under **GPL-3.0**.
