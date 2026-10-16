//! On-disk state: device seed (the root of the per-geohash personas) and
//! user settings. Paths follow XDG: `$XDG_STATE_HOME/backchat/`
//! (default `~/.local/state/backchat/`). The device seed is 256 bits of
//! random, persisted once — losing it rotates every geohash identity.

use rand::RngCore;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub geohash: String,
    #[serde(default)]
    pub relays: Vec<String>,
    /// Recently visited geohash cells for the channel browser's
    /// "your channels" section, newest first, capped.
    #[serde(default)]
    pub channels: Vec<ChannelMemo>,
}

/// One remembered channel visit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChannelMemo {
    pub geohash: String,
    pub last_seen: i64,
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("backchat")
}

/// Load or create (and persist) the device seed.
pub fn load_or_create_device_seed() -> Result<[u8; 32], String> {
    let dir = state_dir();
    let path = dir.join("device-seed.hex");
    if let Ok(hex_str) = fs::read_to_string(&path) {
        let bytes = hex::decode(hex_str.trim())
            .map_err(|e| format!("corrupt device seed {path:?}: {e}"))?;
        return <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| "device seed must be 32 bytes".into());
    }
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    fs::create_dir_all(&dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    fs::write(&path, hex::encode(seed))
        .map_err(|e| format!("write {path:?}: {e}"))?;
    Ok(seed)
}

pub fn load_settings() -> Settings {
    let path = state_dir().join("settings.json");
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    let path = dir.join("settings.json");
    fs::write(&path, serde_json::to_string_pretty(settings).unwrap())
        .map_err(|e| format!("write {path:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_seed_roundtrip() {
        // Point XDG at a throwaway dir.
        let dir = std::env::temp_dir().join(format!("backchat-test-{}", std::process::id()));
        // SAFETY: test-only, single-threaded mutation before any threads spawn.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let _ = std::fs::remove_dir_all(&dir);

        let a = load_or_create_device_seed().unwrap();
        let b = load_or_create_device_seed().unwrap();
        assert_eq!(a, b, "seed must be stable across loads");

        // Settings roundtrip.
        let settings = Settings {
            nickname: "martin".into(),
            geohash: "dr5rs".into(),
            relays: vec!["wss://relay.example".into()],
            channels: vec![ChannelMemo { geohash: "9q8yy".into(), last_seen: 0 }],
        };
        save_settings(&settings).unwrap();
        assert_eq!(load_settings(), settings);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
