//! On-disk state: device seed (the root of the per-geohash personas) and
//! user settings. Paths follow XDG: `$XDG_STATE_HOME/backchat/`
//! (default `~/.local/state/backchat/`). The device seed is 256 bits of
//! random, persisted once. Losing it rotates every geohash identity.

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
    let xdg = std::env::var("XDG_STATE_HOME").ok();
    let home = std::env::var("HOME").ok();
    state_dir_from(xdg, home)
}

/// XDG state dir resolution, split from the process environment so the
/// roundtrip test can exercise the real logic without the env-var races
/// of parallel test threads (two tests touching `set_var` on the same
/// process have been a heisenflake forever).
fn state_dir_from(xdg: Option<String>, home: Option<String>) -> PathBuf {
    xdg.map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("backchat")
}

/// Load or create (and persist) the device seed.
pub fn load_or_create_device_seed() -> Result<[u8; 32], String> {
    load_or_create_device_seed_in(&state_dir())
}

fn load_or_create_device_seed_in(dir: &std::path::Path) -> Result<[u8; 32], String> {
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
    load_settings_in(&state_dir())
}

fn load_settings_in(dir: &std::path::Path) -> Settings {
    let path = dir.join("settings.json");
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    save_settings_in(&state_dir(), settings)
}

fn save_settings_in(dir: &std::path::Path, settings: &Settings) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    let path = dir.join("settings.json");
    fs::write(&path, serde_json::to_string_pretty(settings).unwrap())
        .map_err(|e| format!("write {path:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_seed_roundtrip() {
        // Throwaway dir; the `_in` variants take the path directly, so no
        // process-env mutation (those raced with sibling test threads).
        let dir = std::env::temp_dir().join(format!(
            "backchat-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let a = load_or_create_device_seed_in(&dir).unwrap();
        let b = load_or_create_device_seed_in(&dir).unwrap();
        assert_eq!(a, b, "seed must be stable across loads");

        // Settings roundtrip.
        let settings = Settings {
            nickname: "martin".into(),
            geohash: "dr5rs".into(),
            relays: vec!["wss://relay.example".into()],
            channels: vec![ChannelMemo { geohash: "9q8yy".into(), last_seen: 0 }],
        };
        save_settings_in(&dir, &settings).unwrap();
        assert_eq!(load_settings_in(&dir), settings);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
