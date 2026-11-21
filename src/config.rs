//! Persistent state: identity (device id + long-term key, so iOS recognises the
//! receiver across restarts) and the user's settings.

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

/// Things the user can change from the control panel.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// TCP port of the AirPlay control channel.
    pub port: u16,
    /// Path to ffmpeg; empty = whatever is on PATH.
    pub ffmpeg: String,
    /// Output device name; empty = system default.
    pub audio_device: String,
    pub start_muted: bool,
    pub always_on_top: bool,
    /// Start the receiver as soon as the control panel opens.
    pub auto_start: bool,
    /// Refresh rate advertised to the phone (the most frames per second it may send).
    pub fps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            port: 7000,
            ffmpeg: String::new(),
            audio_device: String::new(),
            start_muted: false,
            always_on_top: false,
            auto_start: true,
            fps: 60,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Config {
    pub name: String,
    /// "AA:BB:CC:DD:EE:FF" - locally administered, random, persisted.
    pub device_id: String,
    /// Hex-encoded 32-byte ed25519 seed.
    pub key_seed: String,
    #[serde(default)]
    pub settings: Settings,
}

impl Config {
    fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir().context("no config directory available")?.join("kplay");
        fs::create_dir_all(&dir)?;
        Ok(dir.join("config.json"))
    }

    /// Load the saved config (creating it on first run). A corrupt file is kept
    /// as `config.json.bad` rather than silently overwritten.
    pub fn load(name_override: Option<&str>) -> Result<Self> {
        let path = Self::path()?;
        let mut cfg = match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("config.json is unreadable ({e}); starting fresh (old file kept as config.json.bad)");
                let _ = fs::rename(&path, path.with_extension("json.bad"));
                Self::generate()
            }),
            Err(_) => Self::generate(),
        };
        if let Some(n) = name_override.map(str::trim).filter(|n| !n.is_empty()) {
            cfg.name = n.to_string();
        }
        cfg.save()?;
        Ok(cfg)
    }

    /// Atomic save (write a temp file, then rename over the real one).
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, &path).context("saving config")
    }

    fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let mut mac = [0u8; 6];
        rng.fill_bytes(&mut mac);
        mac[0] = (mac[0] | 0x02) & 0xFE; // locally administered, unicast
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);

        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "PC".into());
        Config {
            name: format!("KPlay ({host})"),
            device_id: mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"),
            key_seed: hex(&seed),
            settings: Settings::default(),
        }
    }

    pub fn signing_key(&self) -> SigningKey {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = u8::from_str_radix(&self.key_seed[i * 2..i * 2 + 2], 16).unwrap_or(0);
        }
        SigningKey::from_bytes(&seed)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
