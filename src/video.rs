//! Screen-mirroring video: receives the encrypted H.264 stream the iPhone
//! pushes over TCP, decrypts it and decodes it through an ffmpeg process.

use crate::{
    display::{Frame, UiLink},
    h264,
    proc::{self, Child},
};
use aes::Aes128;
use anyhow::{Context, Result};
use ctr::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha512};
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{ChildStdin, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

pub struct VideoReceiver {
    pub port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VideoReceiver {
    pub fn start(aes_key: [u8; 16], stream_id: u64, ffmpeg: PathBuf, ui: UiLink) -> Result<Self> {
        let listener = TcpListener::bind("0.0.0.0:0").context("binding mirror port")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();

        let thread = thread::Builder::new().name("video".into()).spawn(move || {
            if let Err(e) = run(listener, aes_key, stream_id, ffmpeg, ui.clone(), &flag) {
                if !flag.load(Ordering::Relaxed) {
                    log::warn!("video: stream ended: {e:#}");
                }
            }
            if !flag.load(Ordering::Relaxed) {
                ui.disconnected();
            }
        })?;
        Ok(VideoReceiver { port, stop, thread: Some(thread) })
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn key_material(label: &str, stream_id: u64, aes_key: &[u8; 16]) -> [u8; 16] {
    let mut h = Sha512::new();
    h.update(format!("{label}{stream_id}").as_bytes());
    h.update(aes_key);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize()[..16]);
    out
}

/// `read_exact` that wakes up periodically to honour the stop flag.
fn read_full(s: &mut TcpStream, buf: &mut [u8], stop: &AtomicBool) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        match s.read(&mut buf[done..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => done += n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn run(
    listener: TcpListener,
    aes_key: [u8; 16],
    stream_id: u64,
    ffmpeg: PathBuf,
    ui: UiLink,
    stop: &AtomicBool,
) -> Result<()> {
    // wait for the phone to connect
    let mut stream = loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match listener.accept() {
            Ok((s, addr)) => {
                log::info!("video: mirror stream connected from {addr}");
                break s;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.into()),
        }
    };
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_nodelay(true).ok();

    let mut cipher = Aes128Ctr::new(
        &key_material("AirPlayStreamKey", stream_id, &aes_key).into(),
        &key_material("AirPlayStreamIV", stream_id, &aes_key).into(),
    );

    let mut decoder: Option<Decoder> = None;
    let mut params_annexb: Vec<u8> = Vec::new();
    let mut annexb: Vec<u8> = Vec::new();
    let mut header = [0u8; 128];

    loop {
        read_full(&mut stream, &mut header, stop)?;
        let size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        // byte 4 is the packet type; byte 5 carries flags (0x10 marks an IDR frame)
        let kind = u16::from(header[4]);
        if size > 32 * 1024 * 1024 {
            anyhow::bail!("implausible payload size {size}");
        }
        let mut payload = vec![0u8; size];
        read_full(&mut stream, &mut payload, stop)?;

        match kind {
            // encoded frame (encrypted AVCC)
            0 => {
                cipher.apply_keystream(&mut payload);
                let Some(d) = decoder.as_mut() else { continue };
                annexb.clear();
                if !h264::avcc_to_annexb(&payload, &mut annexb) {
                    log::debug!("video: malformed NAL layout, frame skipped");
                    continue;
                }
                if d.stdin.write_all(&annexb).is_err() {
                    log::warn!("video: decoder died, restarting on next parameter set");
                    decoder = None;
                }
            }
            // codec parameters (avcC): (re)start the decoder
            1 => {
                let Some(ps) = h264::parse_avcc(&payload) else {
                    log::warn!("video: unsupported codec configuration ({} bytes) - H.264 only", payload.len());
                    continue;
                };
                let Some((w, h)) = h264::sps_dimensions(&ps.sps) else {
                    log::warn!("video: could not read picture size from SPS");
                    continue;
                };
                params_annexb.clear();
                for nal in [&ps.sps, &ps.pps] {
                    params_annexb.extend_from_slice(&[0, 0, 0, 1]);
                    params_annexb.extend_from_slice(nal);
                }
                if decoder.as_ref().map(|d| (d.w, d.h)) != Some((w, h)) {
                    log::info!("video: {w}x{h}");
                    drop(decoder.take()); // kill the previous decoder first
                    decoder = Some(Decoder::start(&ffmpeg, w, h, ui.clone())?);
                }
                if let Some(d) = decoder.as_mut() {
                    let _ = d.stdin.write_all(&params_annexb);
                }
            }
            // heartbeat (2) / stream report (5) / unknown
            _ => {}
        }
    }
}

struct Decoder {
    child: Child,
    stdin: ChildStdin,
    reader: Option<JoinHandle<()>>,
    w: u32,
    h: u32,
}

impl Decoder {
    fn start(ffmpeg: &PathBuf, w: u32, h: u32, ui: UiLink) -> Result<Decoder> {
        let mut cmd = proc::command(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-flags", "low_delay"])
            .args(["-probesize", "4096", "-analyzeduration", "0", "-threads", "1"])
            .args(["-f", "h264", "-i", "pipe:0", "-an"])
            .args(["-vf", &format!("scale={w}:{h}:flags=fast_bilinear")])
            // `-threads 1` on the output side matters: rawvideo is a frame-threaded
            // encoder, and its thread pipeline would hold back the newest frames.
            .args(["-pix_fmt", "bgra", "-fps_mode", "passthrough", "-threads", "1"])
            .args(["-f", "rawvideo", "-flush_packets", "1", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = Child::new(cmd.spawn().context("starting ffmpeg for video")?);
        let stdin = child.0.stdin.take().unwrap();
        let mut stdout = child.0.stdout.take().unwrap();

        let reader = thread::Builder::new().name("video-read".into()).spawn(move || {
            let words = (w * h) as usize;
            loop {
                let mut px = vec![0u32; words];
                // SAFETY: u32 has no invalid bit patterns and u8 has weaker alignment.
                let bytes = unsafe { std::slice::from_raw_parts_mut(px.as_mut_ptr() as *mut u8, words * 4) };
                if stdout.read_exact(bytes).is_err() {
                    break;
                }
                ui.push_frame(Frame { w, h, px });
            }
        })?;
        Ok(Decoder { child, stdin, reader: Some(reader), w, h })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.child.kill();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Split an Annex-B stream into NAL units (without start codes).
    fn nals(data: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut out = Vec::new();
        for (k, &s) in starts.iter().enumerate() {
            let mut e = starts.get(k + 1).map_or(data.len(), |n| n - 3);
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            out.push(&data[s..e]);
        }
        out
    }

    fn packet(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 128];
        p[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        p[4..6].copy_from_slice(&kind.to_le_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// Plays the part of the iPhone: encrypted AVCC frames over the mirror socket.
    #[test]
    fn mirror_stream_decodes_to_frames() {
        let ffmpeg = proc::find_ffmpeg(None).expect("ffmpeg");
        let tmp = std::env::temp_dir().join("kplay_test.h264");
        let ok = proc::command(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30"])
            .args(["-frames:v", "60", "-c:v", "libx264", "-preset", "ultrafast", "-bf", "0", "-g", "15", "-pix_fmt", "yuv420p"])
            .args(["-f", "h264"])
            .arg(&tmp)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let stream = std::fs::read(&tmp).unwrap();
        let units = nals(&stream);
        let sps = units.iter().find(|n| n[0] & 0x1f == 7).unwrap();
        let pps = units.iter().find(|n| n[0] & 0x1f == 8).unwrap();

        let ui = crate::display::test_link();
        let key = [7u8; 16];
        let id = 1234567890123u64;
        let rx = VideoReceiver::start(key, id, ffmpeg, ui.clone()).unwrap();

        let mut cipher = Aes128Ctr::new(
            &key_material("AirPlayStreamKey", id, &key).into(),
            &key_material("AirPlayStreamIV", id, &key).into(),
        );
        let mut sock = TcpStream::connect(("127.0.0.1", rx.port)).unwrap();

        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
        sock.write_all(&packet(1, &avcc)).unwrap();

        let mut sent = 0u64;
        for nal in units.iter().filter(|n| matches!(n[0] & 0x1f, 1 | 5)) {
            let mut payload = (nal.len() as u32).to_be_bytes().to_vec();
            payload.extend_from_slice(nal);
            cipher.apply_keystream(&mut payload);
            sock.write_all(&packet(0, &payload)).unwrap();
            sent += 1;
            std::thread::sleep(Duration::from_millis(15));
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline && ui.pushed_for_tests() < sent {
            std::thread::sleep(Duration::from_millis(10));
        }
        let got = ui.pushed_for_tests();
        let f = ui.take_frame_for_tests().expect("no frames decoded");
        assert_eq!((f.w, f.h), (320, 240));
        assert!(f.px.iter().any(|&p| p & 0xFFFFFF != 0), "frame is black");
        assert!(got == sent, "only {got} of {sent} frames came through");
    }
}
