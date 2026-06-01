use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::db::CachedActivity;
use crate::types::{ActivityType, Slot};

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
}

// --- Slot-keyed collection of Strava clients ---

/// Holds one Strava client per configured app slot. Mirrors `StravaApps`:
/// slot 1 is always present, slot 2 is `Some` only when the workaround is
/// enabled. Use `client_for_slot` to look up the right client for an athlete.
pub struct StravaClients {
    pub slot_1: Arc<dyn StravaApi>,
    pub slot_2: Option<Arc<dyn StravaApi>>,
}

/// Look up the Strava client for a given slot. Returns an error if `slot_2`
/// is requested but not configured (e.g., an athlete is pinned to slot 2 but
/// the admin removed `STRAVA_CLIENT_ID_2`/`STRAVA_CLIENT_SECRET_2`).
pub fn client_for_slot(clients: &StravaClients, slot: Slot) -> Result<&Arc<dyn StravaApi>> {
    match slot {
        Slot::One => Ok(&clients.slot_1),
        Slot::Two => clients
            .slot_2
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Strava app slot 2 is not configured")),
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
        assert_eq!(cached.distance_km, 10.0);
        assert_eq!(cached.duration_s, 3100);
        // pace: 3000 sec / 10 km = 300 sec/km
        assert_eq!(cached.pace_sec_per_km, Some(300));
        assert_eq!(cached.url, "https://www.strava.com/activities/123");
    }

    /// A no-op `StravaApi` used only to build identifiable `StravaClients`
    /// in lookup tests. Methods are unreachable.
    struct DummyClient {
        tag: &'static str,
    }
    #[async_trait]
    impl StravaApi for DummyClient {
        async fn exchange_code(&self, _code: &str) -> Result<TokenResponse> {
            unreachable!("dummy: {}", self.tag)
        }
        async fn refresh_token(&self, _refresh_token: &str) -> Result<TokenResponse> {
            unreachable!("dummy: {}", self.tag)
        }
        async fn get_activities(
            &self,
            _access_token: &str,
            _after: Option<i64>,
            _before: Option<i64>,
            _per_page: u32,
        ) -> Result<Vec<StravaActivity>> {
            unreachable!("dummy: {}", self.tag)
        }
    }

    #[test]
    fn test_client_for_slot_one_always_ok() {
        let clients = StravaClients {
            slot_1: Arc::new(DummyClient { tag: "one" }),
            slot_2: None,
        };
        assert!(client_for_slot(&clients, Slot::One).is_ok());
    }

    #[test]
    fn test_client_for_slot_two_errs_when_not_configured() {
        let clients = StravaClients {
            slot_1: Arc::new(DummyClient { tag: "one" }),
            slot_2: None,
        };
        match client_for_slot(&clients, Slot::Two) {
            Ok(_) => panic!("expected error when slot 2 not configured"),
            Err(e) => assert!(e.to_string().contains("slot 2")),
        }
    }

    #[test]
    fn test_client_for_slot_two_ok_when_configured() {
        let clients = StravaClients {
            slot_1: Arc::new(DummyClient { tag: "one" }),
            slot_2: Some(Arc::new(DummyClient { tag: "two" })),
        };
        assert!(client_for_slot(&clients, Slot::Two).is_ok());
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
}
