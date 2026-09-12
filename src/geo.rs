//! Offline reverse geocoding for activity start points.
//!
//! Strava's own `location_city` / `location_state` fields were deprecated in
//! 2016 and come back null, so the only usable signal is `start_latlng`.
//! Rather than call a geocoding API on the notification path — another
//! network hop, another rate limit, another thing that times out — we resolve
//! coordinates against the `GeoNames` `cities1000` dataset bundled into the
//! binary by `reverse_geocoder`.
//!
//! The input is the athlete's true start point — privacy zones do not trim
//! what `activity:read_all` returns (see `StravaActivity::start_latlng`) — so
//! the coarseness here is the only thing standing between a home address and
//! the group chat. Nearest-populated-place is deliberate: it resolves a
//! doorstep to a town name, never to coordinates.

use std::sync::OnceLock;

use reverse_geocoder::ReverseGeocoder;

/// Discard a match further than this from the activity's start point.
///
/// The dataset only knows populated places, so a remote trail or an open-water
/// swim can match a "nearest" city hundreds of kilometres away. A confidently
/// wrong place name in the AI prompt is worse than no place name at all.
const MAX_MATCH_KM: f64 = 100.0;

const EARTH_RADIUS_KM: f64 = 6371.0;

/// The k-d tree spans ~150k cities and takes a moment to build, so it is
/// constructed on first use rather than at startup — the bot should not pay
/// for it until an activity actually arrives.
static GEOCODER: OnceLock<ReverseGeocoder> = OnceLock::new();

/// Label a coordinate with its nearest populated place, as `"Singapore, SG"`.
///
/// Returns `None` when the nearest match is too far away to mean anything.
/// Whether the activity has coordinates at all is the caller's concern — see
/// `StravaActivity::start_latlng`.
#[must_use]
pub fn lookup(lat: f64, lon: f64) -> Option<String> {
    let result = GEOCODER
        .get_or_init(ReverseGeocoder::new)
        .search((lat, lon));
    let record = result.record;

    if haversine_km(lat, lon, record.lat, record.lon) > MAX_MATCH_KM {
        return None;
    }

    Some(format!("{}, {}", record.name, record.cc))
}

/// Great-circle distance in kilometres.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (lat1_r, lat2_r) = (lat1.to_radians(), lat2.to_radians());
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();

    let a = (d_lat / 2.0).sin().powi(2) + lat1_r.cos() * lat2_r.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_labels_a_known_city() {
        // Singapore Botanic Gardens.
        assert_eq!(lookup(1.3138, 103.8159), Some("Singapore, SG".to_string()));
    }

    #[test]
    fn test_lookup_returns_none_when_the_nearest_city_is_too_far() {
        // Point Nemo, the oceanic pole of inaccessibility. The nearest
        // populated place is thousands of km away; labelling it would be
        // confidently wrong.
        assert_eq!(lookup(-48.876, -123.393), None);
    }

    #[test]
    fn test_haversine_matches_a_known_distance() {
        // Singapore -> Kuala Lumpur is roughly 315 km.
        let km = haversine_km(1.3521, 103.8198, 3.1390, 101.6869);
        assert!((km - 315.0).abs() < 15.0, "got {km} km");
    }
}
