//! Bonjour/mDNS advertisement so iOS lists us in Screen Mirroring.

use crate::config::{hex, Config};
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use sha2::{Digest, Sha512};

pub const MODEL: &str = "AppleTV3,2";
pub const VERSION: &str = "220.68";
/// AirPlay feature bits (legacy mirroring: video, screen, audio, FairPlay, legacy pairing).
pub const FEATURES: u64 = 0x5A7FFEE6;

pub struct Discovery {
    daemon: ServiceDaemon,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        // unregister politely so the entry disappears from iOS straight away
        let _ = self.daemon.shutdown();
    }
}

fn features_str() -> String {
    format!("0x{:X},0x{:X}", FEATURES & 0xFFFF_FFFF, FEATURES >> 32)
}

pub fn public_key_hex(key: &SigningKey) -> String {
    hex(key.verifying_key().as_bytes())
}

/// A stable UUID-looking id derived from the device id.
fn pi(cfg: &Config) -> String {
    let h = Sha512::digest(cfg.device_id.as_bytes());
    let x = hex(&h[..16]);
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

pub fn airplay_txt(cfg: &Config, pk: &str) -> Vec<(String, String)> {
    [
        ("deviceid", cfg.device_id.clone()),
        ("features", features_str()),
        ("flags", "0x4".into()),
        ("model", MODEL.into()),
        ("pk", pk.into()),
        ("pi", pi(cfg)),
        ("srcvers", VERSION.into()),
        ("vv", "2".into()),
        ("pw", "false".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub fn raop_txt(pk: &str) -> Vec<(String, String)> {
    [
        ("ch", "2"),
        ("cn", "0,1,2,3"),
        ("da", "true"),
        ("et", "0,3,5"),
        ("vv", "2"),
        ("ft", &features_str()),
        ("am", MODEL),
        ("md", "0,1,2"),
        ("rhd", "5.6.0.0"),
        ("pw", "false"),
        ("sf", "0x4"),
        ("sr", "44100"),
        ("ss", "16"),
        ("sv", "false"),
        ("tp", "UDP"),
        ("txtvers", "1"),
        ("vs", VERSION),
        ("vn", "65537"),
        ("pk", pk),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// DNS TXT wire format: a sequence of length-prefixed `key=value` strings.
pub fn txt_bytes(props: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in props {
        let s = format!("{k}={v}");
        out.push(s.len().min(255) as u8);
        out.extend_from_slice(&s.as_bytes()[..s.len().min(255)]);
    }
    out
}

pub fn start(cfg: &Config, key: &SigningKey, port: u16) -> Result<Discovery> {
    let daemon = ServiceDaemon::new().context("starting mDNS responder")?;
    let pk = public_key_hex(key);
    let mac_plain = cfg.device_id.replace(':', "");
    let host = format!("kplay-{}.local.", &mac_plain[6..]);

    let register = |ty: &str, instance: String, props: Vec<(String, String)>| -> Result<()> {
        let refs: Vec<(&str, &str)> = props.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let info = ServiceInfo::new(ty, &instance, &host, "", port, &refs[..])
            .context("building mDNS record")?
            .enable_addr_auto();
        daemon.register(info).context("registering mDNS service")
    };
    register("_airplay._tcp.local.", cfg.name.clone(), airplay_txt(cfg, &pk))?;
    register("_raop._tcp.local.", format!("{mac_plain}@{}", cfg.name), raop_txt(&pk))?;
    Ok(Discovery { daemon })
}
