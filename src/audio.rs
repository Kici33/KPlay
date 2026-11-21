//! Audio pipeline: AAC-ELD frames -> ffmpeg (decode/resample) -> ring buffer -> cpal.
//!
//! Everything fragile lives on one dedicated thread so that [`AudioHandle::reset`]
//! can tear the whole chain down (decoder process, output device, buffered
//! samples) and bring it back up cleanly.

use crate::proc::{self, Child};
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
    process::{ChildStdin, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Sender},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
};

/// Start playing once this much audio is buffered, and stop (re-prime) on underrun.
const PRIME_MS: usize = 60;
/// If the buffer ever grows past this (clock drift, stalled device) trim it...
const MAX_MS: usize = 350;
/// ...down to this, dropping the oldest samples.
const KEEP_MS: usize = 120;

pub enum AudioMsg {
    /// One raw (decrypted) AAC-ELD access unit.
    Packet(Vec<u8>),
    /// Drop whatever is buffered (seek/flush from the sender).
    Flush,
    /// Rebuild decoder + output device.
    Reset,
}

struct Ring {
    buf: VecDeque<f32>,
    primed: bool,
    prime: usize,
    max: usize,
    keep: usize,
}

pub struct AudioShared {
    muted: AtomicBool,
    /// Linear gain requested by the sender (iOS volume slider), f32 bits.
    sender_gain: AtomicU32,
    ring: Mutex<Ring>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct AudioHandle {
    tx: Sender<AudioMsg>,
    shared: Arc<AudioShared>,
}

impl AudioHandle {
    /// device: output device name, empty for the system default.
    pub fn spawn(ffmpeg: PathBuf, start_muted: bool, device: String) -> AudioHandle {
        let shared = Arc::new(AudioShared {
            muted: AtomicBool::new(start_muted),
            sender_gain: AtomicU32::new(1.0f32.to_bits()),
            ring: Mutex::new(Ring { buf: VecDeque::new(), primed: false, prime: 0, max: 0, keep: 0 }),
        });
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || Engine { ffmpeg, device, shared: s, out: None, dec: None, warned: false }.run(rx))
            .expect("spawn audio thread");
        AudioHandle { tx, shared }
    }

    pub fn send(&self, msg: AudioMsg) {
        let _ = self.tx.send(msg);
    }
    pub fn reset(&self) {
        self.send(AudioMsg::Reset);
    }
    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }
    /// Toggle mute; returns the new state.
    pub fn toggle_mute(&self) -> bool {
        !self.shared.muted.fetch_xor(true, Ordering::Relaxed)
    }
    /// iOS volume in dB: -144 = muted, -30 (quiet) ..= 0 (full).
    pub fn set_sender_volume_db(&self, db: f32) {
        let gain = if db <= -144.0 { 0.0 } else { 10f32.powf(db.min(0.0) / 20.0) };
        self.shared.sender_gain.store(gain.to_bits(), Ordering::Relaxed);
    }
}

/// Names of the available output devices (for the settings panel).
pub fn list_output_devices() -> Vec<String> {
    let mut v: Vec<String> = cpal::default_host()
        .output_devices()
        .map(|d| d.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------- engine

struct Engine {
    ffmpeg: PathBuf,
    device: String,
    shared: Arc<AudioShared>,
    out: Option<Output>,
    dec: Option<Decoder>,
    warned: bool,
}

impl Engine {
    fn run(mut self, rx: mpsc::Receiver<AudioMsg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                AudioMsg::Packet(p) => self.packet(&p),
                AudioMsg::Flush => lock(&self.shared.ring).buf.clear(),
                AudioMsg::Reset => {
                    log::info!("audio: reset");
                    self.dec = None; // kills ffmpeg, joins its reader
                    self.out = None; // releases the device
                    let mut r = lock(&self.shared.ring);
                    r.buf.clear();
                    r.primed = false;
                    drop(r);
                    self.warned = false;
                    self.ensure_output();
                }
            }
        }
    }

    fn ensure_output(&mut self) -> bool {
        if self.out.is_none() {
            match Output::open(&self.shared, &self.device) {
                Ok(o) => {
                    log::info!("audio: output {} Hz, {} ch", o.rate, o.channels);
                    self.out = Some(o);
                }
                Err(e) => {
                    if !self.warned {
                        log::error!("audio: cannot open output device: {e:#}");
                        self.warned = true;
                    }
                    return false;
                }
            }
        }
        true
    }

    fn packet(&mut self, payload: &[u8]) {
        if !self.ensure_output() {
            return;
        }
        if self.dec.is_none() {
            let o = self.out.as_ref().unwrap();
            match Decoder::start(&self.ffmpeg, o.rate, o.channels, self.shared.clone()) {
                Ok(d) => self.dec = Some(d),
                Err(e) => {
                    if !self.warned {
                        log::error!("audio: cannot start decoder: {e:#}");
                        self.warned = true;
                    }
                    return;
                }
            }
        }
        if let Some(d) = self.dec.as_mut() {
            let frame = d.mux.block(payload);
            if d.stdin.write_all(&frame).and_then(|_| d.stdin.flush()).is_err() {
                log::warn!("audio: decoder stopped, restarting it");
                self.dec = None;
            }
        }
    }
}

// ---------------------------------------------------------------- output

struct Output {
    _stream: cpal::Stream,
    rate: u32,
    channels: u16,
}

struct Mixer {
    shared: Arc<AudioShared>,
    gain: f32,
    step: f32,
}

impl Mixer {
    fn render(&mut self, out: &mut [f32]) {
        let target = if self.shared.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.shared.sender_gain.load(Ordering::Relaxed))
        };
        let mut r = lock(&self.shared.ring);
        if !r.primed && r.buf.len() >= r.prime {
            r.primed = true;
        }
        for s in out.iter_mut() {
            // short slew so mute/unmute never clicks
            self.gain += (target - self.gain).clamp(-self.step, self.step);
            *s = if r.primed {
                match r.buf.pop_front() {
                    Some(v) => v * self.gain,
                    None => {
                        r.primed = false;
                        0.0
                    }
                }
            } else {
                0.0
            };
        }
    }
}

impl Output {
    fn open(shared: &Arc<AudioShared>, wanted: &str) -> Result<Output> {
        let host = cpal::default_host();
        let chosen = (!wanted.is_empty())
            .then(|| host.output_devices().ok()?.find(|d| d.name().is_ok_and(|n| n == wanted)))
            .flatten();
        if chosen.is_none() && !wanted.is_empty() {
            log::warn!("audio: device '{wanted}' not found, using the system default");
        }
        let device = chosen
            .or_else(|| host.default_output_device())
            .ok_or_else(|| anyhow!("no audio output device available"))?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        let (rate, channels) = (config.sample_rate.0, config.channels);

        {
            let per_ms = rate as usize * channels as usize / 1000;
            let mut r = lock(&shared.ring);
            r.buf.clear();
            r.primed = false;
            r.prime = PRIME_MS * per_ms;
            r.max = MAX_MS * per_ms;
            r.keep = KEEP_MS * per_ms;
        }

        let mut mixer = Mixer {
            shared: shared.clone(),
            gain: 0.0,
            step: 1.0 / (rate as f32 * 0.02),
        };
        let err = |e| log::error!("audio: output stream error: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| mixer.render(data),
                err,
                None,
            )?,
            cpal::SampleFormat::I16 => {
                let mut tmp = Vec::new();
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        tmp.resize(data.len(), 0.0);
                        mixer.render(&mut tmp);
                        for (d, s) in data.iter_mut().zip(&tmp) {
                            *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    },
                    err,
                    None,
                )?
            }
            other => return Err(anyhow!("unsupported output sample format {other:?}")),
        };
        stream.play()?;
        Ok(Output { _stream: stream, rate, channels })
    }
}

// ---------------------------------------------------------------- decoder

struct Decoder {
    child: Child,
    stdin: ChildStdin,
    reader: Option<JoinHandle<()>>,
    mux: MkvStream,
}

impl Decoder {
    fn start(ffmpeg: &PathBuf, rate: u32, channels: u16, shared: Arc<AudioShared>) -> Result<Decoder> {
        let mut cmd = proc::command(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-flags", "low_delay"])
            .args(["-probesize", "2048", "-analyzeduration", "0"])
            .args(["-f", "matroska", "-i", "pipe:0", "-vn"])
            .args(["-af", "aresample=async=0"])
            .args(["-f", "f32le", "-ar", &rate.to_string(), "-ac", &channels.to_string()])
            .args(["-threads", "1", "-flush_packets", "1", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = Child::new(cmd.spawn().context("starting ffmpeg for audio")?);
        let mut stdin = child.0.stdin.take().unwrap();
        let mux = MkvStream::new();
        stdin.write_all(&mux.header()).context("feeding ffmpeg")?;
        let mut stdout = child.0.stdout.take().unwrap();

        let reader = thread::Builder::new().name("audio-read".into()).spawn(move || {
            let mut raw = [0u8; 8192];
            let mut carry: Vec<u8> = Vec::new();
            let mut samples: Vec<f32> = Vec::new();
            loop {
                let n = match stdout.read(&mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                carry.extend_from_slice(&raw[..n]);
                let usable = carry.len() / 4 * 4;
                samples.clear();
                samples.extend(
                    carry[..usable]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
                carry.drain(..usable);

                let mut r = lock(&shared.ring);
                r.buf.extend(samples.iter().copied());
                if r.buf.len() > r.max {
                    let excess = r.buf.len() - r.keep;
                    r.buf.drain(..excess);
                    log::debug!("audio: buffer overflow, dropped {excess} samples");
                }
            }
        })?;
        Ok(Decoder { child, stdin, reader: Some(reader), mux })
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

// ---------------------------------------------------------------- Matroska framing

/// AudioSpecificConfig for AAC-ELD, 44.1 kHz, stereo, 480-sample frames (what
/// AirPlay mirroring uses). Same bytes UxPlay hands to its decoder.
const ELD_ASC: [u8; 4] = [0xF8, 0xE8, 0x50, 0x00];
const ELD_RATE: u64 = 44_100;
const ELD_SPF: u64 = 480;
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(id.len() + 8 + body.len());
    v.extend_from_slice(id);
    v.push(0x01);
    v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    v.extend_from_slice(body);
    v
}

fn ebml_cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// Feeds raw access units to ffmpeg as a live Matroska stream. A stock ffmpeg
/// decodes AAC-ELD fine when the codec config arrives as container extradata
/// (it does not work when carried inside LATM).
struct MkvStream {
    n: u64,
    cluster_ms: u64,
}

impl MkvStream {
    fn new() -> Self {
        MkvStream { n: 0, cluster_ms: 0 }
    }

    fn header(&self) -> Vec<u8> {
        let head = ebml(
            &[0x1A, 0x45, 0xDF, 0xA3],
            &ebml_cat(&[
                ebml(&[0x42, 0x86], &[1]),
                ebml(&[0x42, 0xF7], &[1]),
                ebml(&[0x42, 0xF2], &[4]),
                ebml(&[0x42, 0xF3], &[8]),
                ebml(&[0x42, 0x82], b"matroska"),
                ebml(&[0x42, 0x87], &[4]),
                ebml(&[0x42, 0x85], &[2]),
            ]),
        );
        let info = ebml(&[0x15, 0x49, 0xA9, 0x66], &ebml(&[0x2A, 0xD7, 0xB1], &1_000_000u32.to_be_bytes()));
        let audio = ebml(
            &[0xE1],
            &ebml_cat(&[ebml(&[0xB5], &(ELD_RATE as f64).to_be_bytes()), ebml(&[0x9F], &[2])]),
        );
        let track = ebml(
            &[0xAE],
            &ebml_cat(&[
                ebml(&[0xD7], &[1]),
                ebml(&[0x73, 0xC5], &[1]),
                ebml(&[0x83], &[2]),
                ebml(&[0x86], b"A_AAC"),
                ebml(&[0x63, 0xA2], &ELD_ASC),
                audio,
            ]),
        );
        let tracks = ebml(&[0x16, 0x54, 0xAE, 0x6B], &track);
        let mut out = head;
        out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        out.extend_from_slice(&UNKNOWN_SIZE);
        out.extend(info);
        out.extend(tracks);
        out
    }

    /// One raw access unit as container bytes (opens a new cluster every ~0.7 s).
    fn block(&mut self, payload: &[u8]) -> Vec<u8> {
        let ms = self.n * ELD_SPF * 1000 / ELD_RATE;
        let mut out = Vec::with_capacity(payload.len() + 40);
        if self.n % 64 == 0 {
            self.cluster_ms = ms;
            out.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75]);
            out.extend_from_slice(&UNKNOWN_SIZE);
            out.extend(ebml(&[0xE7], &(ms as u32).to_be_bytes()));
        }
        let rel = (ms - self.cluster_ms) as i16;
        let mut body = vec![0x81];
        body.extend_from_slice(&rel.to_be_bytes());
        body.push(0x80); // keyframe
        body.extend_from_slice(payload);
        out.extend(ebml(&[0xA3], &body));
        self.n += 1;
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The engine must survive resets, junk input and mute toggles without dying.
    #[test]
    fn engine_survives_reset_and_mute() {
        let h = AudioHandle::spawn(PathBuf::from("ffmpeg"), false, String::new());
        assert!(!h.is_muted());
        assert!(h.toggle_mute());
        assert!(h.is_muted());
        assert!(!h.toggle_mute());
        h.reset();
        for i in 0..30u8 {
            h.send(AudioMsg::Packet(vec![i; 120]));
        }
        h.reset();
        h.send(AudioMsg::Flush);
        std::thread::sleep(std::time::Duration::from_millis(600));
        // engine thread is still alive iff its channel is still open
        h.tx.send(AudioMsg::Flush).expect("audio thread died");
    }

    #[test]
    #[ignore = "converts a captured KPLAY_DUMP_AUDIO file to Matroska"]
    fn captured_to_mkv() {
        let b = std::fs::read(std::env::temp_dir().join("a2.bin")).unwrap();
        let mut mux = MkvStream::new();
        let mut out = mux.header();
        let mut i = 0;
        while i + 4 <= b.len() {
            let l = u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            if i + 4 + l > b.len() {
                break;
            }
            out.extend(mux.block(&b[i + 4..i + 4 + l]));
            i += 4 + l;
        }
        std::fs::write(std::env::temp_dir().join("cap.mkv"), out).unwrap();
    }
}