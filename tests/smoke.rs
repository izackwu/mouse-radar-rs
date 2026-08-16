#![allow(clippy::float_cmp)]

use async_trait::async_trait;
use std::sync::Arc;

use mouse_radar_rs::ai::AiClient;
use mouse_radar_rs::comment;
use mouse_radar_rs::config::AiConfig;
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

    async fn get_activity_detail(
        &self,
        _access_token: &str,
        _activity_id: i64,
    ) -> anyhow::Result<mouse_radar_rs::strava::StravaActivityDetail> {
        Ok(mouse_radar_rs::strava::StravaActivityDetail::default())
    }
}

#[tokio::test]
async fn test_full_pipeline_with_mock() {
    // Setup: temp database
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());

    // Insert a test athlete
    db.run(|conn| db::upsert_athlete(conn, 12345, "testuser", "acc", "ref", 9_999_999_999))
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
            db::upsert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
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

/// Stub AI client: returns a canned completion, or fails on demand.
struct StubAi {
    response: String,
    fail: bool,
}

#[async_trait]
impl AiClient for StubAi {
    async fn comment(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
        if self.fail {
            anyhow::bail!("stub failure");
        }
        Ok(self.response.clone())
    }
}

fn test_ai_config() -> AiConfig {
    AiConfig {
        api_key: "k".into(),
        base_url: "http://localhost".into(),
        model: "m".into(),
        history_limit: 30,
        timeout_seconds: 20,
        max_chars: 280,
        system_prompt: None,
    }
}

fn seed_db_with_history(db: &Arc<Db>, count: i64) -> CachedActivity {
    db.run(|conn| db::upsert_athlete(conn, 1, "zack", "acc", "ref", 9_999_999_999))
        .unwrap();
    for i in 0..count {
        db.run(|conn| {
            db::cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 100 + i,
                    athlete_id: 1,
                    title: "Prior".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 8.0,
                    duration_s: 2400,
                    pace_sec_per_km: Some(300),
                    start_date_local: format!("2026-08-{:02}T08:00:00Z", i + 1),
                    start_date: format!("2026-08-{:02}T08:00:00Z", i + 1),
                    url: "u".into(),
                },
            )
        })
        .unwrap();
    }

    let current = CachedActivity {
        activity_id: 999,
        athlete_id: 1,
        title: "Morning shakeout".into(),
        activity_type: ActivityType::Run,
        distance_km: 12.4,
        duration_s: 3872,
        pace_sec_per_km: Some(312),
        start_date_local: "2026-08-16T06:28:00Z".into(),
        start_date: "2026-08-16T06:28:00Z".into(),
        url: "u".into(),
    };
    // The poller caches before notifying, so the current activity is present.
    db.run(|conn| db::cache_activity(conn, &current)).unwrap();
    current
}

#[tokio::test]
async fn test_compose_comment_returns_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
    let current = seed_db_with_history(&db, 5);

    let ai: Arc<dyn AiClient> = Arc::new(StubAi {
        response: "  Third run this week and the longest yet.  ".into(),
        fail: false,
    });
    let strava: Arc<dyn StravaApi> = Arc::new(MockStrava {
        activities: vec![],
        token_response: None,
    });

    let got = comment::compose_comment(
        &ai,
        &strava,
        &db,
        &test_ai_config(),
        "acc",
        "zack",
        &current,
    )
    .await
    .unwrap();

    assert_eq!(
        got,
        Some("Third run this week and the longest yet.".to_string())
    );
}

#[tokio::test]
async fn test_compose_comment_skips_when_no_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
    // Zero prior activities: only the current one is cached.
    let current = seed_db_with_history(&db, 0);

    let ai: Arc<dyn AiClient> = Arc::new(StubAi {
        response: "should never be used".into(),
        fail: false,
    });
    let strava: Arc<dyn StravaApi> = Arc::new(MockStrava {
        activities: vec![],
        token_response: None,
    });

    let got = comment::compose_comment(
        &ai,
        &strava,
        &db,
        &test_ai_config(),
        "acc",
        "zack",
        &current,
    )
    .await
    .unwrap();

    assert_eq!(got, None);
}

#[tokio::test]
async fn test_compose_comment_propagates_ai_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
    let current = seed_db_with_history(&db, 5);

    let ai: Arc<dyn AiClient> = Arc::new(StubAi {
        response: String::new(),
        fail: true,
    });
    let strava: Arc<dyn StravaApi> = Arc::new(MockStrava {
        activities: vec![],
        token_response: None,
    });

    let got = comment::compose_comment(
        &ai,
        &strava,
        &db,
        &test_ai_config(),
        "acc",
        "zack",
        &current,
    )
    .await;

    // The caller logs and drops; the notification is already delivered.
    assert!(got.is_err());
}

#[tokio::test]
async fn test_compose_comment_skips_blank_completion() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
    let current = seed_db_with_history(&db, 5);

    let ai: Arc<dyn AiClient> = Arc::new(StubAi {
        response: "   \n  ".into(),
        fail: false,
    });
    let strava: Arc<dyn StravaApi> = Arc::new(MockStrava {
        activities: vec![],
        token_response: None,
    });

    let got = comment::compose_comment(
        &ai,
        &strava,
        &db,
        &test_ai_config(),
        "acc",
        "zack",
        &current,
    )
    .await
    .unwrap();

    assert_eq!(got, None);
}
