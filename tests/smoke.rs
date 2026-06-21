#![allow(clippy::float_cmp)]

use async_trait::async_trait;
use std::sync::Arc;

use mouse_radar_rs::db::{self, CachedActivity, Db};
use mouse_radar_rs::formatting;
use mouse_radar_rs::strava::{StravaActivity, StravaApi, StravaAthleteSummary, TokenResponse};
use mouse_radar_rs::types::ActivityType;

/// Mock Strava client that returns canned data.
struct MockStrava {
    activities: Vec<StravaActivity>,
    token_response: Option<TokenResponse>,
}

#[async_trait]
impl StravaApi for MockStrava {
    async fn exchange_code(&self, _code: &str) -> anyhow::Result<TokenResponse> {
        Ok(self.token_response.clone().unwrap())
    }

    async fn refresh_token(&self, _refresh_token: &str) -> anyhow::Result<TokenResponse> {
        Ok(self.token_response.clone().unwrap())
    }

    async fn get_activities(
        &self,
        _access_token: &str,
        _after: Option<i64>,
        _before: Option<i64>,
        _per_page: u32,
    ) -> anyhow::Result<Vec<StravaActivity>> {
        Ok(self.activities.clone())
    }
}

#[tokio::test]
async fn test_full_pipeline_with_mock() {
    // Setup: temp database
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());

    // Insert a test athlete
    db.run(|conn| db::insert_athlete(conn, 12345, "testuser", "acc", "ref", 9_999_999_999))
        .unwrap();

    // Create mock Strava with one activity
    let mock = Arc::new(MockStrava {
        activities: vec![StravaActivity {
            id: 999,
            athlete: StravaAthleteSummary { id: 12345 },
            name: "Morning Run".into(),
            activity_type: ActivityType::Run,
            distance: 5000.0,
            moving_time: 1500,
            elapsed_time: 1600,
            start_date: "2026-05-14T08:00:00Z".into(),
            start_date_local: "2026-05-14T16:00:00Z".into(),
        }],
        token_response: Some(TokenResponse {
            access_token: "acc".into(),
            refresh_token: "ref".into(),
            expires_at: 9_999_999_999,
            expires_in: 21600,
        }),
    });

    let strava_client: Arc<dyn StravaApi> = mock;

    // Fetch activities via mock
    let activities = strava_client
        .get_activities("acc", None, None, 50)
        .await
        .unwrap();

    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0].name, "Morning Run");
    assert_eq!(activities[0].activity_type, ActivityType::Run);

    // Verify to_cached conversion
    let cached = mouse_radar_rs::strava::to_cached(&activities[0]);
    assert_eq!(cached.distance_km, 5.0);
    assert_eq!(cached.title, "Morning Run");
    assert_eq!(cached.url, "https://www.strava.com/activities/999");

    // Cache it in the DB
    db.run(|conn| db::cache_activity(conn, &cached)).unwrap();

    // Verify stats (activity date is 2026-05-14)
    let monday = chrono::NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
    let first_of_month = chrono::NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
    let week = db.run(|conn| db::get_week_km(conn, 12345, monday)).unwrap();
    let month = db
        .run(|conn| db::get_month_km(conn, 12345, first_of_month))
        .unwrap();

    assert_eq!(week, 5.0);
    assert_eq!(month, 5.0);

    // Verify message formatting
    let msg = formatting::format_activity_message(
        "testuser",
        "Morning Run",
        ActivityType::Run,
        5.0,
        Some(300),
        1600,
        5.0,
        5.0,
        "https://www.strava.com/activities/999",
        false,
        false,
    );

    assert!(msg.contains("testuser ran 5.0 km"));
    assert!(msg.contains("Week: 5.0 km"));
    assert!(msg.contains("Month: 5.0 km"));
}

#[test]
fn test_cache_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    // First session
    {
        let db = Db::open(db_path.to_str().unwrap()).unwrap();
        db.run(|conn| {
            db::insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
            db::cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Run".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 8.0,
                    duration_s: 2400,
                    pace_sec_per_km: Some(300),
                    start_date_local: "2026-05-14T08:00:00Z".into(),
                    start_date: "2026-05-14T08:00:00Z".into(),
                    url: "https://strava.com/activities/1".into(),
                },
            )
        })
        .unwrap();
    }

    // Second session (reopen)
    {
        let db = Db::open(db_path.to_str().unwrap()).unwrap();
        let monday = chrono::NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
        let km = db.run(|conn| db::get_week_km(conn, 1, monday)).unwrap();
        assert_eq!(km, 8.0);
        let latest = db
            .run(|conn| db::get_latest_activity(conn, 1))
            .unwrap()
            .unwrap();
        assert_eq!(latest.title, "Run");
    }
}
