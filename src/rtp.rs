//! Audio transport: encrypted RTP over UDP (AES-128-CBC, payload = AAC-ELD).

use crate::audio::{AudioHandle, AudioMsg};
use aes::Aes128;
use anyhow::{Context, Result};
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use std::{
    io,
    net::UdpSocket,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct AudioReceiver {
    pub data_port: u16,
    pub control_port: u16,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

/// AirPlay `ct` value for AAC-ELD, the codec used for mirroring audio.
pub const CT_AAC_ELD: u64 = 8;

impl AudioReceiver {
    pub fn start(aes_key: [u8; 16], aes_iv: [u8; 16], ct: u64, audio: AudioHandle) -> Result<Self> {
        let data = UdpSocket::bind("0.0.0.0:0").context("binding audio data port")?;
        let control = UdpSocket::bind("0.0.0.0:0").context("binding audio control port")?;
        data.set_read_timeout(Some(Duration::from_millis(200)))?;
        control.set_read_timeout(Some(Duration::from_millis(200)))?;
        let (data_port, control_port) = (data.local_addr()?.port(), control.local_addr()?.port());
        let stop = Arc::new(AtomicBool::new(false));

        if ct != CT_AAC_ELD {
            log::warn!("audio: unsupported codec ct={ct} (only AAC-ELD mirroring audio is handled); audio disabled");
        }

        let flag = stop.clone();
        let t_data = thread::Builder::new().name("audio-udp".into()).spawn(move || {
            let mut buf = [0u8; 4096];
            let mut last_seq: Option<u16> = None;
            while !flag.load(Ordering::Relaxed) {
                let n = match data.recv(&mut buf) {
                    Ok(n) => n,
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
                    Err(_) => continue,
                };
                if ct != CT_AAC_ELD || n <= 12 || buf[1] & 0x7f != 0x60 {
                    continue;
                }
                let seq = u16::from_be_bytes([buf[2], buf[3]]);
                if let Some(prev) = last_seq {
                    // drop duplicates / late packets (wrapping compare)
                    if (seq.wrapping_sub(prev) as i16) <= 0 {
                        continue;
                    }
                }
                last_seq = Some(seq);

                let payload = &mut buf[12..n];
                let enc = payload.len() / 16 * 16; // trailing partial block is sent in the clear
                if enc > 0 {
                    let _ = cbc::Decryptor::<Aes128>::new(&aes_key.into(), &aes_iv.into())
                        .decrypt_padded_mut::<NoPadding>(&mut payload[..enc]);
                }
                audio.send(AudioMsg::Packet(payload.to_vec()));
            }
        })?;

        // The control socket carries sync packets and retransmissions; we only
        // need to keep it open (and drain it) so the sender is satisfied.
        let flag = stop.clone();
        let t_ctl = thread::Builder::new().name("audio-ctl".into()).spawn(move || {
            let mut buf = [0u8; 2048];
            while !flag.load(Ordering::Relaxed) {
                let _ = control.recv(&mut buf);
            }
        })?;

        Ok(AudioReceiver { data_port, control_port, stop, threads: vec![t_data, t_ctl] })
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
