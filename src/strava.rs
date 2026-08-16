use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::db::CachedActivity;
use crate::types::ActivityType;

// --- OAuth types ---

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub expires_in: i64,
}

// --- Activity types from Strava API ---

#[derive(Debug, Clone, Deserialize)]
pub struct StravaActivity {
    pub id: i64,
    pub athlete: StravaAthleteSummary,
    pub name: String,
    #[serde(rename = "type")]
    pub activity_type: ActivityType,
    pub distance: f64,
    pub moving_time: i64,
    pub elapsed_time: i64,
    pub start_date: String,
    pub start_date_local: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StravaAthleteSummary {
    pub id: i64,
}

/// Detailed activity from `GET /activities/{id}`.
///
/// Every field is optional: Strava omits keys depending on device, sport
/// type, and whether the athlete wore a heart-rate strap. Serde defaults
/// keep an unexpected shape from failing the whole parse.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StravaActivityDetail {
    #[serde(default)]
    pub average_heartrate: Option<f64>,
    #[serde(default)]
    pub max_heartrate: Option<f64>,
    #[serde(default)]
    pub total_elevation_gain: Option<f64>,
    #[serde(default)]
    pub average_cadence: Option<f64>,
    #[serde(default)]
    pub splits_metric: Vec<StravaSplit>,
    #[serde(default)]
    pub laps: Vec<StravaLap>,
    #[serde(default)]
    pub best_efforts: Vec<StravaBestEffort>,
}

/// One of Strava's uniform per-kilometre splits.
#[derive(Debug, Clone, Deserialize)]
pub struct StravaSplit {
    pub split: i64,
    pub distance: f64,
    pub moving_time: i64,
    pub elapsed_time: i64,
    #[serde(default)]
    pub average_speed: Option<f64>,
    #[serde(default)]
    pub average_heartrate: Option<f64>,
    #[serde(default)]
    pub elevation_difference: Option<f64>,
}

/// A lap as recorded by the athlete's device — manual button press or
/// auto-lap. Carries max HR and cadence, which splits do not.
#[derive(Debug, Clone, Deserialize)]
pub struct StravaLap {
    pub lap_index: i64,
    pub distance: f64,
    pub moving_time: i64,
    pub elapsed_time: i64,
    #[serde(default)]
    pub average_speed: Option<f64>,
    #[serde(default)]
    pub max_speed: Option<f64>,
    #[serde(default)]
    pub average_heartrate: Option<f64>,
    #[serde(default)]
    pub max_heartrate: Option<f64>,
    #[serde(default)]
    pub average_cadence: Option<f64>,
    #[serde(default)]
    pub total_elevation_gain: Option<f64>,
}

/// A standard-distance effort. `pr_rank` is non-null only when the effort is
/// a top-3 all-time result for that athlete — Strava's own arithmetic, and
/// the only figure in the prompt the model does not have to compute.
#[derive(Debug, Clone, Deserialize)]
pub struct StravaBestEffort {
    pub name: String,
    pub elapsed_time: i64,
    #[serde(default)]
    pub pr_rank: Option<i64>,
}

// --- Trait for testability ---

#[async_trait]
pub trait StravaApi: Send + Sync {
    async fn exchange_code(&self, code: &str) -> Result<TokenResponse>;
    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenResponse>;
    async fn get_activities(
        &self,
        access_token: &str,
        after: Option<i64>,
        before: Option<i64>,
        per_page: u32,
    ) -> Result<Vec<StravaActivity>>;
    async fn get_activity_detail(
        &self,
        access_token: &str,
        activity_id: i64,
    ) -> Result<StravaActivityDetail>;
}

// --- Real implementation ---

pub struct StravaClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

impl StravaClient {
    #[must_use]
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            client_id,
            client_secret,
        }
    }
}

#[async_trait]
impl StravaApi for StravaClient {
    async fn exchange_code(&self, code: &str) -> Result<TokenResponse> {
        let resp = self
            .http
            .post("https://www.strava.com/oauth/token")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Strava OAuth exchange failed: {}", body);
        }

        Ok(resp.json().await?)
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenResponse> {
        let resp = self
            .http
            .post("https://www.strava.com/oauth/token")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Strava token refresh failed: {}", body);
        }

        Ok(resp.json().await?)
    }

    async fn get_activities(
        &self,
        access_token: &str,
        after: Option<i64>,
        before: Option<i64>,
        per_page: u32,
    ) -> Result<Vec<StravaActivity>> {
        let mut query = vec![("per_page", per_page.to_string())];
        if let Some(a) = after {
            query.push(("after", a.to_string()));
        }
        if let Some(b) = before {
            query.push(("before", b.to_string()));
        }

        let resp = self
            .http
            .get("https://www.strava.com/api/v3/athlete/activities")
            .header("Authorization", format!("Bearer {}", access_token))
            .query(&query)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Strava API error ({}): {}", status, body);
        }

        Ok(resp.json().await?)
    }

    async fn get_activity_detail(
        &self,
        access_token: &str,
        activity_id: i64,
    ) -> Result<StravaActivityDetail> {
        let resp = self
            .http
            .get(format!(
                "https://www.strava.com/api/v3/activities/{}",
                activity_id
            ))
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Strava detail API error ({}): {}", status, body);
        }

        Ok(resp.json().await?)
    }
}

// --- Helper to convert StravaActivity -> CachedActivity ---

#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn to_cached(activity: &StravaActivity) -> CachedActivity {
    let pace = if activity.moving_time > 0 && activity.distance > 0.0 {
        let sec_per_km = activity.moving_time as f64 / (activity.distance / 1000.0);
        Some(sec_per_km.round() as i64)
    } else {
        None
    };

    CachedActivity {
        activity_id: activity.id,
        athlete_id: activity.athlete.id,
        title: activity.name.clone(),
        activity_type: activity.activity_type,
        distance_km: activity.distance / 1000.0,
        duration_s: activity.elapsed_time,
        pace_sec_per_km: pace,
        start_date_local: activity.start_date_local.clone(),
        start_date: activity.start_date.clone(),
        url: format!("https://www.strava.com/activities/{}", activity.id),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn test_to_cached_computes_pace() {
        let act = StravaActivity {
            id: 123,
            athlete: StravaAthleteSummary { id: 456 },
            name: "Run".into(),
            activity_type: ActivityType::Run,
            distance: 10000.0, // 10 km
            moving_time: 3000, // 50 min = 3000 seconds
            elapsed_time: 3100,
            start_date: "2024-01-01T00:00:00Z".into(),
            start_date_local: "2024-01-01T08:00:00Z".into(),
        };

        let cached = to_cached(&act);
        assert_eq!(cached.activity_id, 123);
        assert_eq!(cached.start_date, "2024-01-01T00:00:00Z");
        assert_eq!(cached.distance_km, 10.0);
        assert_eq!(cached.duration_s, 3100);
        // pace: 3000 sec / 10 km = 300 sec/km
        assert_eq!(cached.pace_sec_per_km, Some(300));
        assert_eq!(cached.url, "https://www.strava.com/activities/123");
    }

    #[test]
    fn test_to_cached_zero_distance_no_pace() {
        let act = StravaActivity {
            id: 1,
            athlete: StravaAthleteSummary { id: 1 },
            name: "Walk".into(),
            activity_type: ActivityType::Walk,
            distance: 0.0,
            moving_time: 600,
            elapsed_time: 600,
            start_date: "2024-01-01T00:00:00Z".into(),
            start_date_local: "2024-01-01T00:00:00Z".into(),
        };

        let cached = to_cached(&act);
        assert_eq!(cached.pace_sec_per_km, None);
    }

    #[test]
    fn test_detail_deserializes_full_payload() {
        let body = r#"{
            "average_heartrate": 152.4,
            "max_heartrate": 171.0,
            "total_elevation_gain": 240.0,
            "average_cadence": 89.0,
            "splits_metric": [
                {"split": 1, "distance": 1000.0, "moving_time": 331,
                 "elapsed_time": 331, "average_speed": 3.02,
                 "average_heartrate": 141.0, "elevation_difference": 12.0}
            ],
            "laps": [
                {"lap_index": 1, "distance": 400.0, "moving_time": 90,
                 "elapsed_time": 90, "average_speed": 4.44, "max_speed": 5.1,
                 "average_heartrate": 168.0, "max_heartrate": 174.0,
                 "average_cadence": 93.0, "total_elevation_gain": 2.0}
            ],
            "best_efforts": [
                {"name": "5k", "elapsed_time": 1264, "pr_rank": 2},
                {"name": "1k", "elapsed_time": 240, "pr_rank": null}
            ]
        }"#;
        let d: StravaActivityDetail = serde_json::from_str(body).unwrap();
        assert_eq!(d.max_heartrate, Some(171.0));
        assert_eq!(d.splits_metric.len(), 1);
        assert_eq!(d.splits_metric[0].average_heartrate, Some(141.0));
        assert_eq!(d.laps.len(), 1);
        assert_eq!(d.laps[0].max_heartrate, Some(174.0));
        assert_eq!(d.best_efforts[0].pr_rank, Some(2));
        assert_eq!(d.best_efforts[1].pr_rank, None);
    }

    #[test]
    fn test_detail_deserializes_minimal_payload() {
        // A watch with no HR strap, no laps, non-run activity: Strava omits
        // most keys entirely. This must not fail the parse.
        let d: StravaActivityDetail = serde_json::from_str("{}").unwrap();
        assert!(d.average_heartrate.is_none());
        assert!(d.splits_metric.is_empty());
        assert!(d.laps.is_empty());
        assert!(d.best_efforts.is_empty());
    }
}
