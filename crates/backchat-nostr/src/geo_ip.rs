//! Coarse location estimate for the channel browser's "from your location"
//! ladder. One IP-geolocation call, city-level accuracy at best: enough
//! for the region/province/city rungs, which is precisely where upstream's
//! GPS ladder is usable on mobile. The fix is never rendered as precise;
//! the UI labels the section "est.".

use serde::Deserialize;

/// Where we (probably) are, to city precision.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoFix {
    pub lat: f64,
    pub lon: f64,
    /// Place labels for the ladder rungs (may be empty strings).
    pub city: String,
    pub region: String,
    pub country: String,
}

#[derive(Debug, Deserialize)]
struct IpWhoIs {
    latitude: Option<f64>,
    longitude: Option<f64>,
    city: Option<String>,
    region: Option<String>,
    country: Option<String>,
    success: Option<bool>,
}

/// Fetch an approximate location from ipwho.is (keyless, HTTPS).
/// Blocking with a hard timeout: run off the UI thread.
pub fn fetch_approx_location() -> Option<GeoFix> {
    let body = ureq::get("https://ipwho.is/")
        .timeout(std::time::Duration::from_secs(6))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let body: IpWhoIs = serde_json::from_str(&body).ok()?;
    if body.success == Some(false) {
        return None;
    }
    Some(GeoFix {
        lat: body.latitude?,
        lon: body.longitude?,
        city: body.city.unwrap_or_default(),
        region: body.region.unwrap_or_default(),
        country: body.country.unwrap_or_default(),
    })
}
