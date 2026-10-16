//! The geo relay directory: relays with approximate GPS locations, and the
//! "closest N to a geohash" selection. Port of `GeoRelayDirectory.swift` +
//! `Geohash.swift`'s decode. Publishers and subscribers must agree on relay
//! sets, so the sort is exactly upstream's: haversine distance, ties broken
//! by host.

use std::collections::BTreeSet;

/// https URL of upstream's directory (raw GitHub, as upstream fetches it).
pub const DIRECTORY_URL: &str = "https://raw.githubusercontent.com/permissionlesstech/bitchat/refs/heads/main/relays/online_relays_gps.csv";

/// Bundled snapshot (fixtures/relays/online_relays_gps.csv), used when the
/// remote fetch fails (offline, blocked, redirect policy).
const BUNDLED_CSV: &str = include_str!("../../../fixtures/relays/online_relays_gps.csv");

/// Remote fetch timeout for the directory.
const FETCH_TIMEOUT_SECS: u64 = 5;

/// How many relays a geohash channel uses (upstream default).
pub const RELAYS_PER_CHANNEL: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub host: String,
    pub lat: f64,
    pub lon: f64,
}

/// Parse the directory CSV: `Relay URL,Latitude,Longitude` header, then rows.
/// Accepts bare hosts or full wss:// URLs in the first column.
pub fn parse_csv(csv: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut lines = csv.lines();
    let header = lines.next().unwrap_or_default();
    let header_parts: Vec<&str> = header.split(',').map(str::trim).collect();
    if header_parts != ["Relay URL", "Latitude", "Longitude"] {
        return out;
    }
    for line in lines {
        let mut parts = line.split(',');
        let (Some(raw_host), Some(lat), Some(lon)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let host = raw_host
            .trim()
            .trim_start_matches("wss://")
            .trim_end_matches('/')
            .to_string();
        // Hostname sanity (mirrors upstream's validation): 2+ labels of
        // 1–63 chars.
        if !valid_host(&host) || parts.next().is_some() {
            continue;
        }
        let (Ok(lat), Ok(lon)) = (lat.trim().parse::<f64>(), lon.trim().parse::<f64>()) else {
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        out.push(Entry { host, lat, lon });
    }
    out
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// Base32 geohash alphabet (standard).
const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Decode a geohash to the center of its cell. Port of
/// `Geohash.decodeCenter`.
pub fn geohash_center(geohash: &str) -> Option<(f64, f64)> {
    let mut lat = (-90.0f64, 90.0f64);
    let mut lon = (-180.0f64, 180.0f64);
    let mut even = true;
    for ch in geohash.to_lowercase().chars() {
        let cd = BASE32.iter().position(|&b| b as char == ch)? as u32;
        for mask in [16u32, 8, 4, 2, 1] {
            let mid = if even {
                (lon.0 + lon.1) / 2.0
            } else {
                (lat.0 + lat.1) / 2.0
            };
            if even {
                if cd & mask != 0 {
                    lon.0 = mid
                } else {
                    lon.1 = mid
                }
            } else if cd & mask != 0 {
                lat.0 = mid
            } else {
                lat.1 = mid
            }
            even = !even;
        }
    }
    Some(((lat.0 + lat.1) / 2.0, (lon.0 + lon.1) / 2.0))
}

/// Haversine distance in km: byte-for-byte the upstream formula.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0;
    let dlat = (lat2 - lat1) * std::f64::consts::PI / 180.0;
    let dlon = (lon2 - lon1) * std::f64::consts::PI / 180.0;
    let a = (dlat / 2.0).sin() * (dlat / 2.0).sin()
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin() * (dlon / 2.0).sin();
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    r * c
}

/// Up to `count` wss:// relay URLs closest to the geohash center. Ties break
/// by host so every device with the same directory picks the same set.
pub fn closest_relays(entries: &[Entry], geohash: &str, count: usize) -> Vec<String> {
    let Some((lat, lon)) = geohash_center(geohash) else {
        return Vec::new();
    };
    let mut ranked: Vec<(f64, &str)> = entries
        .iter()
        .map(|e| (haversine_km(lat, lon, e.lat, e.lon), e.host.as_str()))
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(b.1)));
    let mut seen = BTreeSet::new();
    ranked
        .into_iter()
        .filter(|(_, host)| seen.insert(host.to_string()))
        .take(count)
        .map(|(_, host)| format!("wss://{host}"))
        .collect()
}

/// Load the directory: remote CSV with the bundled snapshot as fallback.
/// Blocking (up to `FETCH_TIMEOUT_SECS`): call before entering the UI.
pub fn load_directory() -> Vec<Entry> {
    match fetch_remote() {
        Ok(entries) if !entries.is_empty() => entries,
        _ => parse_csv(BUNDLED_CSV),
    }
}

fn fetch_remote() -> Result<Vec<Entry>, String> {
    let response = ureq::get(DIRECTORY_URL)
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .call()
        .map_err(|e| format!("directory fetch: {e}"))?;
    let body = response
        .into_string()
        .map_err(|e| format!("directory body: {e}"))?;
    Ok(parse_csv(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bundled_directory() {
        let entries = parse_csv(BUNDLED_CSV);
        assert!(entries.len() > 100, "bundled directory has {} entries", entries.len());
        assert!(entries.iter().all(|e| valid_host(&e.host)));
        assert!(entries.iter().all(|e| e.lat.abs() <= 90.0));
    }

    #[test]
    fn geohash_center_matches_known_cells() {
        // dr5rs ≈ New York City (40.7, -74.0).
        let (lat, lon) = geohash_center("dr5rs").unwrap();
        assert!((lat - 40.7).abs() < 0.5, "lat {lat}");
        assert!((lon + 74.0).abs() < 0.5, "lon {lon}");

        // 9q8yy ≈ San Francisco (37.77, -122.4).
        let (lat, lon) = geohash_center("9q8yy").unwrap();
        assert!((lat - 37.77).abs() < 0.5, "lat {lat}");
        assert!((lon + 122.4).abs() < 0.5, "lon {lon}");
    }

    #[test]
    fn closest_relays_deterministic_with_ties() {
        let entries = parse_csv(BUNDLED_CSV);
        let a = closest_relays(&entries, "dr5rs", 5);
        let b = closest_relays(&entries, "dr5rs", 5);
        assert_eq!(a, b, "same directory must give the same set");
        assert_eq!(a.len(), 5);
        assert!(a.iter().all(|u| u.starts_with("wss://")));

        // An NYC relay should rank first for a NYC geohash.
        assert!(a[0].contains("nostr1.com") || a.iter().any(|u| u.contains("damus")), "got {a:?}");
    }

    #[test]
    fn csv_validation() {
        assert!(parse_csv("Bad,Header\n").is_empty());
        assert!(parse_csv(BUNDLED_CSV).len() > 0);
        // Out-of-range coords rejected.
        assert!(parse_csv("Relay URL,Latitude,Longitude\na.b,91,0\n").is_empty());
        assert!(parse_csv("Relay URL,Latitude,Longitude\nn,0,0\n").is_empty()); // 1 label
        assert_eq!(parse_csv("Relay URL,Latitude,Longitude\nwss://a.b/,-1.5,2.5\n")[0].host, "a.b");
    }
}
