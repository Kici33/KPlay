//! The AirPlay control channel: a single TCP port speaking an HTTP/RTSP hybrid.

use crate::{
    audio::{AudioHandle, AudioMsg},
    config::Config,
    discovery::{self, MODEL, VERSION},
    display::UiLink,
    fairplay,
    pairing::Pairing,
    rtp::AudioReceiver,
    video::VideoReceiver,
};
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use plist::{Dictionary, Value};
use sha2::{Digest, Sha512};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Cursor, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
};

pub struct Ctx {
    pub cfg: Config,
    pub key: SigningKey,
    pub ffmpeg: PathBuf,
    pub audio: AudioHandle,
    pub ui: UiLink,
    session: Mutex<Option<Session>>,
    next_conn: AtomicU64,
}

impl Ctx {
    pub fn new(cfg: Config, key: SigningKey, ffmpeg: PathBuf, audio: AudioHandle, ui: UiLink) -> Arc<Self> {
        Arc::new(Ctx { cfg, key, ffmpeg, audio, ui, session: Mutex::new(None), next_conn: AtomicU64::new(1) })
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, Option<Session>> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One active phone. Dropping it stops all of its streams.
struct Session {
    owner: u64,
    video: Option<VideoReceiver>,
    audio: Option<AudioReceiver>,
    _timing: Option<UdpSocket>,
}

pub fn serve(ctx: Arc<Ctx>, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let ctx = ctx.clone();
        let _ = thread::Builder::new().name("rtsp".into()).spawn(move || {
            let id = ctx.next_conn.fetch_add(1, Ordering::Relaxed);
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            log::debug!("conn {id}: opened from {peer}");
            if let Err(e) = handle(&ctx, stream, id) {
                log::debug!("conn {id}: {e:#}");
            }
            // The phone went away: if it owned the active session, tear it down.
            let mut s = ctx.sessions();
            if s.as_ref().is_some_and(|x| x.owner == id) {
                *s = None;
                drop(s);
                ctx.ui.disconnected();
                ctx.audio.send(AudioMsg::Flush);
                log::info!("session ended");
            }
            log::debug!("conn {id}: closed");
        });
    }
}

// ------------------------------------------------------------------ HTTP-ish plumbing

struct Request {
    method: String,
    path: String,
    proto: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    ctype: Option<&'static str>,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

impl Reply {
    fn ok() -> Self {
        Reply { status: 200, ctype: None, body: Vec::new(), headers: Vec::new() }
    }
    fn status(status: u16) -> Self {
        Reply { status, ..Self::ok() }
    }
    fn data(ctype: &'static str, body: Vec<u8>) -> Self {
        Reply { ctype: Some(ctype), body, ..Self::ok() }
    }
    fn plist(dict: Dictionary) -> Self {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &Value::Dictionary(dict)).expect("plist serialisation");
        Self::data("application/x-apple-binary-plist", buf)
    }
}

fn read_request(r: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let (method, path, proto) = (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    );
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(Request { method, path, proto, headers, body }))
}

fn write_reply(w: &mut TcpStream, req: &Request, rep: &Reply) -> std::io::Result<()> {
    let proto = if req.proto.starts_with("RTSP") { "RTSP/1.0" } else { "HTTP/1.1" };
    let reason = match rep.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    };
    let mut head = format!("{proto} {} {reason}\r\nServer: AirTunes/{VERSION}\r\n", rep.status);
    if let Some(c) = req.headers.get("cseq") {
        head.push_str(&format!("CSeq: {c}\r\n"));
    }
    if let Some(t) = rep.ctype {
        head.push_str(&format!("Content-Type: {t}\r\n"));
    }
    for (k, v) in &rep.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", rep.body.len()));
    let mut out = head.into_bytes();
    out.extend_from_slice(&rep.body);
    w.write_all(&out)?;
    w.flush()
}

// ------------------------------------------------------------------ connection handling

struct Conn {
    id: u64,
    pairing: Pairing,
    fp_keymsg: Option<[u8; 164]>,
    aes_key: [u8; 16],
    aes_iv: [u8; 16],
}

fn handle(ctx: &Arc<Ctx>, stream: TcpStream, id: u64) -> Result<()> {
    stream.set_nodelay(true).ok();
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut conn = Conn { id, pairing: Pairing::default(), fp_keymsg: None, aes_key: [0; 16], aes_iv: [0; 16] };

    while let Some(req) = read_request(&mut reader)? {
        log::debug!("conn {id}: {} {} ({} bytes)", req.method, req.path, req.body.len());
        let rep = route(ctx, &mut conn, &req);
        write_reply(&mut writer, &req, &rep)?;
    }
    Ok(())
}

fn route(ctx: &Arc<Ctx>, conn: &mut Conn, req: &Request) -> Reply {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/info") => info(ctx),
        ("POST", "/pair-setup") => {
            if req.body.len() != 32 {
                return Reply::status(400);
            }
            Reply::data("application/octet-stream", ctx.key.verifying_key().as_bytes().to_vec())
        }
        ("POST", "/pair-verify") => pair_verify(ctx, conn, req),
        ("POST", "/fp-setup") => fp_setup(conn, req),
        ("POST", "/feedback") => Reply::ok(),
        ("OPTIONS", _) => Reply {
            headers: vec![("Public", "SETUP, RECORD, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER".into())],
            ..Reply::ok()
        },
        ("SETUP", _) => setup(ctx, conn, req),
        ("RECORD", _) => Reply {
            headers: vec![("Audio-Latency", "11025".into()), ("Audio-Jack-Status", "connected; type=analog".into())],
            ..Reply::ok()
        },
        ("FLUSH", _) => {
            ctx.audio.send(AudioMsg::Flush);
            Reply::ok()
        }
        ("TEARDOWN", _) => teardown(ctx, conn, req),
        ("GET_PARAMETER", _) => {
            let body = String::from_utf8_lossy(&req.body);
            if body.contains("volume") {
                Reply::data("text/parameters", b"volume: 0.000000\r\n".to_vec())
            } else {
                Reply::ok()
            }
        }
        ("SET_PARAMETER", _) => {
            if req.headers.get("content-type").is_some_and(|t| t.starts_with("text/parameters")) {
                for line in String::from_utf8_lossy(&req.body).lines() {
                    if let Some(v) = line.strip_prefix("volume:") {
                        if let Ok(db) = v.trim().parse::<f32>() {
                            ctx.audio.set_sender_volume_db(db);
                        }
                    }
                }
            }
            Reply::ok()
        }
        ("GET", _) => Reply::status(404),
        _ => Reply::ok(), // /command, /audioMode, ... : acknowledge and ignore
    }
}

fn info(ctx: &Ctx) -> Reply {
    let pk = discovery::public_key_hex(&ctx.key);
    let cfg = &ctx.cfg;
    let mut d = Dictionary::new();
    let s = |v: &str| Value::String(v.to_string());
    let u = |v: u64| Value::Integer(v.into());

    d.insert("txtAirPlay".into(), Value::Data(discovery::txt_bytes(&discovery::airplay_txt(cfg, &pk))));
    d.insert("txtRAOP".into(), Value::Data(discovery::txt_bytes(&discovery::raop_txt(&pk))));
    d.insert("deviceID".into(), s(&cfg.device_id));
    d.insert("macAddress".into(), s(&cfg.device_id));
    d.insert("pk".into(), Value::Data(ctx.key.verifying_key().as_bytes().to_vec()));
    d.insert("features".into(), u(discovery::FEATURES));
    d.insert("name".into(), s(&cfg.name));
    d.insert("vv".into(), u(2));
    d.insert("statusFlags".into(), u(68));
    d.insert("keepAliveLowPower".into(), Value::Boolean(true));
    d.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(true));
    d.insert("sourceVersion".into(), s(VERSION));
    d.insert("model".into(), s(MODEL));
    d.insert("initialVolume".into(), Value::Real(-12.0));

    let mut latencies = Vec::new();
    let mut formats = Vec::new();
    for ty in [100u64, 101] {
        let mut l = Dictionary::new();
        l.insert("type".into(), u(ty));
        l.insert("audioType".into(), s("default"));
        l.insert("inputLatencyMicros".into(), u(0));
        l.insert("outputLatencyMicros".into(), Value::Boolean(false));
        latencies.push(Value::Dictionary(l));

        let mut f = Dictionary::new();
        f.insert("type".into(), u(ty));
        f.insert("audioInputFormats".into(), u(0x3fffffc));
        f.insert("audioOutputFormats".into(), u(0x3fffffc));
        formats.push(Value::Dictionary(f));
    }
    d.insert("audioLatencies".into(), Value::Array(latencies));
    d.insert("audioFormats".into(), Value::Array(formats));

    let mut disp = Dictionary::new();
    disp.insert("uuid".into(), s("e0ff8a27-6738-3d56-8a16-cc53aacee925"));
    disp.insert("widthPhysical".into(), u(0));
    disp.insert("heightPhysical".into(), u(0));
    disp.insert("width".into(), u(1920));
    disp.insert("height".into(), u(1080));
    disp.insert("widthPixels".into(), u(1920));
    disp.insert("heightPixels".into(), u(1080));
    disp.insert("rotation".into(), Value::Boolean(false));
    let fps = cfg.settings.fps.clamp(24, 240);
    disp.insert("refreshRate".into(), Value::Real(1.0 / fps as f64));
    disp.insert("maxFPS".into(), u(fps as u64));
    disp.insert("overscanned".into(), Value::Boolean(false));
    disp.insert("features".into(), u(14));
    d.insert("displays".into(), Value::Array(vec![Value::Dictionary(disp)]));
    Reply::plist(d)
}

fn pair_verify(ctx: &Ctx, conn: &mut Conn, req: &Request) -> Reply {
    match req.body.first() {
        Some(1) => match conn.pairing.verify_step1(&req.body, &ctx.key) {
            Some(out) => Reply::data("application/octet-stream", out),
            None => Reply::status(400),
        },
        // Step 2 carries the client's signature. We run transient pairing and
        // accept any client on the LAN, so there is nothing to check.
        Some(0) if req.body.len() == 68 => {
            log::debug!("pair-verify step 2: client signature valid = {:?}", conn.pairing.verify_step2(&req.body));
            Reply::data("application/octet-stream", Vec::new())
        }
        
        _ => Reply::status(400),
    }
}

fn fp_setup(conn: &mut Conn, req: &Request) -> Reply {
    match req.body.len() {
        16 => match fairplay::setup(&req.body) {
            Some(r) => Reply::data("application/octet-stream", r.to_vec()),
            None => {
                log::error!("FairPlay setup rejected (version byte {:#x}): unsupported client protocol", req.body[4]);
                Reply::status(501)
            }
        },
        164 => match fairplay::handshake(&req.body) {
            Some(r) => {
                conn.fp_keymsg = Some(req.body[..].try_into().unwrap());
                Reply::data("application/octet-stream", r.to_vec())
            }
            None => Reply::status(400),
        },
        _ => Reply::status(400),
    }
}

fn uint(v: &Value) -> Option<u64> {
    v.as_unsigned_integer().or_else(|| v.as_signed_integer().map(|i| i as u64))
}

fn setup(ctx: &Arc<Ctx>, conn: &mut Conn, req: &Request) -> Reply {
    let Ok(Value::Dictionary(root)) = Value::from_reader(Cursor::new(&req.body)) else {
        return Reply::status(400);
    };
    let mut resp = Dictionary::new();

    // SETUP #1: keys + timing; this starts a new session.
    if let (Some(Value::Data(ekey)), Some(Value::Data(eiv))) = (root.get("ekey"), root.get("eiv")) {
        let (Some(keymsg), true) = (conn.fp_keymsg, eiv.len() == 16) else {
            log::error!("SETUP before FairPlay handshake / bad eiv");
            return Reply::status(400);
        };
        let Some(mut key) = fairplay::decrypt_key(&keymsg, ekey) else {
            return Reply::status(400);
        };
        if let Some(shared) = conn.pairing.shared_secret {
            let mut h = Sha512::new();
            h.update(key);
            h.update(shared);
            key.copy_from_slice(&h.finalize()[..16]);
        }
        conn.aes_key = key;
        conn.aes_iv.copy_from_slice(eiv);

        let device = root.get("name").and_then(Value::as_string).unwrap_or("iOS device").to_string();
        log::info!("{device} connected (user agent: {})", req.headers.get("user-agent").map_or("?", |s| s));

        // we don't act as an NTP peer, but give the phone a port to point at
        let timing = UdpSocket::bind("0.0.0.0:0").ok();
        let timing_port = timing.as_ref().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port());

        // a new phone replaces whoever was connected before
        *ctx.sessions() = Some(Session { owner: conn.id, video: None, audio: None, _timing: timing });
        ctx.audio.send(AudioMsg::Flush);
        ctx.ui.connected(device);

        resp.insert("timingPort".into(), Value::Integer((timing_port as u64).into()));
        resp.insert("eventPort".into(), Value::Integer(0u64.into()));
    }

    // SETUP #2: the actual streams.
    if let Some(Value::Array(streams)) = root.get("streams") {
        let mut out = Vec::new();
        let mut guard = ctx.sessions();
        let Some(session) = guard.as_mut().filter(|s| s.owner == conn.id) else {
            log::error!("stream SETUP without an active session");
            return Reply::status(500);
        };
        for s in streams.iter().filter_map(Value::as_dictionary) {
            let ty = s.get("type").and_then(uint).unwrap_or(0);
            let mut o = Dictionary::new();
            o.insert("type".into(), Value::Integer(ty.into()));
            match ty {
                110 => {
                    let id = s.get("streamConnectionID").and_then(uint).unwrap_or(0);
                    session.video = None;
                    match VideoReceiver::start(conn.aes_key, id, ctx.ffmpeg.clone(), ctx.ui.clone()) {
                        Ok(v) => {
                            o.insert("dataPort".into(), Value::Integer((v.port as u64).into()));
                            session.video = Some(v);
                        }
                        Err(e) => {
                            log::error!("cannot start video: {e:#}");
                            return Reply::status(500);
                        }
                    }
                }
                96 => {
                    let ct = s.get("ct").and_then(uint).unwrap_or(0);
                    session.audio = None;
                    match AudioReceiver::start(conn.aes_key, conn.aes_iv, ct, ctx.audio.clone()) {
                        Ok(a) => {
                            o.insert("dataPort".into(), Value::Integer((a.data_port as u64).into()));
                            o.insert("controlPort".into(), Value::Integer((a.control_port as u64).into()));
                            session.audio = Some(a);
                        }
                        Err(e) => {
                            log::error!("cannot start audio: {e:#}");
                            return Reply::status(500);
                        }
                    }
                }
                other => log::warn!("SETUP: unknown stream type {other}"),
            }
            out.push(Value::Dictionary(o));
        }
        resp.insert("streams".into(), Value::Array(out));
    }
    Reply::plist(resp)
}

fn teardown(ctx: &Arc<Ctx>, conn: &Conn, req: &Request) -> Reply {
    let mut video = false;
    let mut audio = false;
    if let Ok(Value::Dictionary(root)) = Value::from_reader(Cursor::new(&req.body)) {
        if let Some(Value::Array(streams)) = root.get("streams") {
            for t in streams.iter().filter_map(Value::as_dictionary).filter_map(|d| d.get("type").and_then(uint)) {
                video |= t == 110;
                audio |= t == 96;
            }
        }
    }
    let mut guard = ctx.sessions();
    let owned = guard.as_ref().is_some_and(|s| s.owner == conn.id);
    if owned && (video || audio) && !(video && audio) {
        let s = guard.as_mut().unwrap();
        if video {
            s.video = None;
            ctx.ui.disconnected();
        }
        if audio {
            s.audio = None;
            ctx.audio.send(AudioMsg::Flush);
        }
    } else if owned {
        *guard = None;
        ctx.ui.disconnected();
        ctx.audio.send(AudioMsg::Flush);
        log::info!("session ended");
    }
    Reply::ok()
}

/// Bind the control port, falling back to any free port if it is taken.
pub fn bind(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", port))
        .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))
        .context("binding the AirPlay control port")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::Aes128;
    use ctr::cipher::{KeyIvInit, StreamCipher};
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    use rand::rngs::OsRng;
    use x25519_dalek::{EphemeralSecret, PublicKey};

    struct Client(TcpStream);

    impl Client {
        fn call(&mut self, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
            let req = format!("{method} {path} RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n", body.len());
            self.0.write_all(req.as_bytes()).unwrap();
            self.0.write_all(body).unwrap();
            let mut r = BufReader::new(self.0.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let status: u16 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            let mut len = 0;
            loop {
                line.clear();
                r.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            (status, body)
        }
    }

    fn dict(body: &[u8]) -> Dictionary {
        Value::from_reader(Cursor::new(body)).unwrap().into_dictionary().unwrap()
    }

    #[test]
    fn full_handshake_and_session_lifecycle() {
        let ui = crate::display::test_link();
        let cfg = Config {
            name: "Test Receiver".into(),
            device_id: "02:11:22:33:44:55".into(),
            key_seed: "07".repeat(32),
            settings: Default::default(),
        };
        let key = cfg.signing_key();
        let audio = AudioHandle::spawn(PathBuf::from("ffmpeg"), true, String::new());
        let ctx = Ctx::new(cfg, key.clone(), PathBuf::from("ffmpeg"), audio, ui);
        let listener = bind(0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let c2 = ctx.clone();
        thread::spawn(move || serve(c2, listener));

        let mut c = Client(TcpStream::connect(("127.0.0.1", port)).unwrap());

        // /info
        let (st, body) = c.call("GET", "/info", b"");
        assert_eq!(st, 200);
        let info = dict(&body);
        assert_eq!(info.get("name").and_then(Value::as_string), Some("Test Receiver"));
        assert_eq!(info.get("pk").and_then(Value::as_data), Some(&key.verifying_key().as_bytes()[..]));

        // pair-setup returns the long-term public key
        let (st, pk) = c.call("POST", "/pair-setup", &[0x42; 32]);
        assert_eq!((st, &pk[..]), (200, &key.verifying_key().as_bytes()[..]));

        // pair-verify: check the server's signature the way an iPhone would
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let my_pub = PublicKey::from(&secret);
        let mut body = vec![1, 0, 0, 0];
        body.extend_from_slice(my_pub.as_bytes());
        body.extend_from_slice(&[0x55; 32]);
        let (st, resp) = c.call("POST", "/pair-verify", &body);
        assert_eq!((st, resp.len()), (200, 96));
        let their_pub: [u8; 32] = resp[..32].try_into().unwrap();
        let shared = secret.diffie_hellman(&PublicKey::from(their_pub)).to_bytes();
        let derive = |salt: &str| {
            let mut h = Sha512::new();
            h.update(salt.as_bytes());
            h.update(shared);
            let mut o = [0u8; 16];
            o.copy_from_slice(&h.finalize()[..16]);
            o
        };
        let mut sig = resp[32..].to_vec();
        ctr::Ctr128BE::<Aes128>::new(&derive("Pair-Verify-AES-Key").into(), &derive("Pair-Verify-AES-IV").into())
            .apply_keystream(&mut sig);
        let mut signed = their_pub.to_vec();
        signed.extend_from_slice(my_pub.as_bytes());
        let vk = VerifyingKey::from_bytes(key.verifying_key().as_bytes()).unwrap();
        vk.verify(&signed, &Signature::from_slice(&sig).unwrap()).expect("server signature must verify");
        let mut step2 = vec![0, 0, 0, 0];
        step2.extend_from_slice(&[0u8; 64]);
        assert_eq!(c.call("POST", "/pair-verify", &step2).0, 200);

        // FairPlay: both phases
        let mut fp1 = vec![b'F', b'P', b'L', b'Y', 3, 1, 1, 0, 0, 0, 0, 4, 2, 0, 1, 0xbb];
        assert_eq!(fp1.len(), 16);
        let (st, r) = c.call("POST", "/fp-setup", &fp1);
        assert_eq!((st, r.len(), &r[..4]), (200, 142, &b"FPLY"[..]));
        fp1[4] = 9; // unsupported version -> clean refusal, not a crash
        assert_eq!(c.call("POST", "/fp-setup", &fp1).0, 501);
        let mut fp2 = vec![0u8; 164];
        fp2[..12].copy_from_slice(&[b'F', b'P', b'L', b'Y', 3, 1, 3, 0, 0, 0, 0, 0x98]);
        let (st, r) = c.call("POST", "/fp-setup", &fp2);
        assert_eq!((st, r.len(), &r[..4]), (200, 32, &b"FPLY"[..]));

        // SETUP #1 (keys) + #2 (video + audio streams)
        let mut d = Dictionary::new();
        d.insert("ekey".into(), Value::Data(vec![1; 72]));
        d.insert("eiv".into(), Value::Data(vec![2; 16]));
        d.insert("name".into(), Value::String("Test iPhone".into()));
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &Value::Dictionary(d)).unwrap();
        let (st, r) = c.call("SETUP", "rtsp://x/1", &buf);
        assert_eq!(st, 200);
        assert!(dict(&r).contains_key("timingPort"));

        let stream = |ty: u64, extra: &[(&str, u64)]| {
            let mut s = Dictionary::new();
            s.insert("type".into(), Value::Integer(ty.into()));
            for (k, v) in extra {
                s.insert((*k).into(), Value::Integer((*v).into()));
            }
            Value::Dictionary(s)
        };
        let mut d = Dictionary::new();
        d.insert(
            "streams".into(),
            Value::Array(vec![stream(110, &[("streamConnectionID", 99)]), stream(96, &[("ct", 8), ("controlPort", 5000)])]),
        );
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &Value::Dictionary(d)).unwrap();
        let (st, r) = c.call("SETUP", "rtsp://x/1", &buf);
        assert_eq!(st, 200);
        let resp = dict(&r);
        let streams = resp.get("streams").and_then(Value::as_array).unwrap();
        let ports: Vec<u64> = streams
            .iter()
            .filter_map(Value::as_dictionary)
            .map(|s| s.get("dataPort").and_then(uint).unwrap())
            .collect();
        assert_eq!(ports.len(), 2);
        TcpStream::connect(("127.0.0.1", ports[0] as u16)).expect("video port accepts the mirror connection");
        assert!(ctx.sessions().is_some());

        // volume + teardown
        let (st, _) = c.0.try_clone().map(|_| c.call("SET_PARAMETER", "rtsp://x/1", b"volume: -20.0\r\n")).unwrap();
        assert_eq!(st, 200);
        assert_eq!(c.call("TEARDOWN", "rtsp://x/1", b"").0, 200);
        assert!(ctx.sessions().is_none());
    }
}